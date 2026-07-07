//! The spike UI: path bar, filter box, virtualized file table, perf bar.
//!
//! Rendering strategy: `ScrollArea::show_rows` lays out only the visible
//! rows, and each row is drawn with the painter directly (three clipped text
//! calls + a background rect) instead of nested widget layouts. That keeps
//! per-frame cost proportional to *visible* rows — the entry count is
//! irrelevant to frame time, which is the whole premise of the spike.

use crate::fonts::icon;
use crate::icons::IconCache;
use crate::telemetry::Telemetry;
use crate::theme;
use eframe::egui::{self, vec2, Align2, Color32, FontId, Rect, Sense, Stroke};
use fx_core::{
    filter_sorted, format_size, format_unix, sort_indices, spawn_enumerate, spawn_generate,
    synthetic_entries, Batch, Entry, GenMsg, SortKey,
};
use fx_index::{FileIndex, IndexMsg, SearchOutput, TailMsg};
use fx_platform::shell::{spawn_shell_worker, MenuItem, ShellEvent, ShellRequest};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const ROW_H: f32 = 24.0;
const LARGE_ROW_H: f32 = 44.0;
const GRID_TILE_W: f32 = 116.0;
const GRID_TILE_H: f32 = 106.0;
const GRID_ICON: f32 = 56.0;
const SIZE_W: f32 = 100.0;
const MODIFIED_W: f32 = 140.0;
const PAD: f32 = 8.0;
const SYNTH_COUNT: usize = 100_000;
const RAM_COUNT: usize = 500_000;
const DRIVE_HITS_CAP: usize = 100_000;
/// Full drive scans wait for the query to settle this long; incremental
/// narrowing (sub-ms) runs on every keystroke with no delay.
const DRIVE_DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    Path,
    Filter,
    DriveSearch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    List,
    LargeList,
    Grid,
}

impl ViewMode {
    fn as_str(self) -> &'static str {
        match self {
            ViewMode::List => "list",
            ViewMode::LargeList => "large",
            ViewMode::Grid => "grid",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "large" => ViewMode::LargeList,
            "grid" => ViewMode::Grid,
            _ => ViewMode::List,
        }
    }
}

/// Everything one tab of browsing owns: the directory, its streamed
/// entries, sort/filter state, and selection. `sorted` is the master index:
/// all entries in display order, re-sorted only when the data or sort key
/// changes; `visible` is a selection from it, so filtering never re-sorts.
struct BrowseTab {
    path_input: String,
    current_dir: Option<PathBuf>,
    entries: Vec<Entry>,
    sorted: Vec<u32>,
    visible: Vec<u32>,
    query: String,
    sort: SortKey,
    ascending: bool,
    sort_dirty: bool,
    /// The (lowercased) query `visible` was computed for, if still valid for
    /// the current `sorted`. Lets extended queries rescan only the previous
    /// survivors, and makes duplicate change events free.
    computed_query: String,
    computed_valid: bool,
    /// Multi-selection (entry indices) + the anchor for shift-ranges.
    selected: HashSet<u32>,
    select_anchor: Option<u32>,

    // Streaming enumeration
    rx: Option<Receiver<Batch>>,
    enum_started: Option<Instant>,
    first_batch_ms: Option<f32>,
    enum_elapsed: Option<Duration>,

    // Perf, per tab
    filter_ms: f32,
    sort_ms: f32,
    recompute_reason: Option<&'static str>,
}

impl BrowseTab {
    fn new() -> Self {
        Self {
            path_input: String::new(),
            current_dir: None,
            entries: Vec::new(),
            sorted: Vec::new(),
            visible: Vec::new(),
            query: String::new(),
            sort: SortKey::Name,
            ascending: true,
            sort_dirty: false,
            computed_query: String::new(),
            computed_valid: false,
            selected: HashSet::new(),
            select_anchor: None,
            rx: None,
            enum_started: None,
            first_batch_ms: None,
            enum_elapsed: None,
            filter_ms: 0.0,
            sort_ms: 0.0,
            recompute_reason: None,
        }
    }

    /// Short label for the tab bar.
    fn title(&self) -> String {
        match &self.current_dir {
            Some(dir) => dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.display().to_string()),
            None => "RAM".into(),
        }
    }

    /// Replace the entry set, handing any large old allocation to a worker
    /// to free — dropping 500k strings on the UI thread costs ~17 ms.
    fn replace_entries(&mut self, new_entries: Vec<Entry>) {
        let old = std::mem::replace(&mut self.entries, new_entries);
        if old.len() > 10_000 {
            std::thread::spawn(move || drop(old));
        }
        self.sorted.clear();
        self.visible.clear();
        self.sort_dirty = true;
        self.computed_valid = false;
        self.selected.clear();
        self.select_anchor = None;
    }

    fn navigate(&mut self, path: PathBuf, telem: &mut Telemetry) {
        telem.log("nav", format!("open {}", path.display()));
        self.path_input = path.display().to_string();
        self.current_dir = Some(path.clone());
        self.replace_entries(Vec::new());
        self.enum_started = Some(Instant::now());
        self.first_batch_ms = None;
        self.enum_elapsed = None;
        self.rx = Some(spawn_enumerate(path));
    }

    /// Drain this tab's enumeration stream, if any.
    fn drain_enum(&mut self, telem: &mut Telemetry, status: &mut String) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        for msg in rx.try_iter() {
            match msg {
                Batch::Entries(mut batch, at) => {
                    // Worker-side stamp: when the data was ready, not when
                    // the UI's frame cadence got around to draining.
                    self.first_batch_ms.get_or_insert(at.as_secs_f32() * 1000.0);
                    self.entries.append(&mut batch);
                    self.sort_dirty = true;
                    self.computed_valid = false;
                }
                Batch::Done {
                    total,
                    errors,
                    elapsed,
                } => {
                    telem.log(
                        "enum-done",
                        format!(
                            "{} | {total} entries in {:.1} ms, first batch {:.1} ms, {errors} unreadable",
                            self.path_input,
                            elapsed.as_secs_f32() * 1000.0,
                            self.first_batch_ms.unwrap_or(0.0),
                        ),
                    );
                    if errors > 0 {
                        *status = format!(
                            "{errors} entr{} could not be read (access denied?)",
                            if errors == 1 { "y" } else { "ies" }
                        );
                    }
                    self.recompute_reason = Some("enum-done");
                    self.enum_elapsed = Some(elapsed);
                    done = true;
                }
                Batch::Error(e) => {
                    telem.log("error", &e);
                    *status = e;
                    done = true;
                }
            }
        }
        if done {
            self.rx = None;
        }
    }

    /// Bring `sorted` and `visible` up to date. Runs every frame for the
    /// active tab; costs nothing when nothing changed.
    fn refresh_view(&mut self, telem: &mut Telemetry) {
        if self.sort_dirty {
            let out = sort_indices(&self.entries, self.sort, self.ascending);
            self.sorted = out.indices;
            self.sort_ms = out.sort_ms;
            self.sort_dirty = false;
            self.computed_valid = false;
        }

        let query_lower = self.query.to_lowercase();
        if self.computed_valid && query_lower == self.computed_query {
            self.recompute_reason = None; // duplicate event; nothing changed
            return;
        }

        let prev = (self.computed_valid && !self.computed_query.is_empty())
            .then_some((self.computed_query.as_str(), self.visible.as_slice()));
        let out = filter_sorted(&self.entries, &self.sorted, &query_lower, prev);
        self.visible = out.visible;
        self.filter_ms = out.filter_ms;
        self.computed_query = query_lower;
        self.computed_valid = true;

        if let Some(reason) = self.recompute_reason.take() {
            telem.log(
                "recompute",
                format!(
                    "{reason}{} query={:?} -> {}/{} visible, filter {:.2} ms, sort {:.2} ms",
                    if out.incremental {
                        " (incremental)"
                    } else {
                        ""
                    },
                    self.query,
                    self.visible.len(),
                    self.entries.len(),
                    self.filter_ms,
                    self.sort_ms,
                ),
            );
        }
    }

    fn sort_by(&mut self, key: SortKey) {
        if self.sort == key {
            self.ascending = !self.ascending;
        } else {
            self.sort = key;
            self.ascending = key == SortKey::Name;
        }
        self.recompute_reason = Some("sort");
        self.sort_dirty = true;
    }

    /// Sortable column header for the list views.
    fn header(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);

        let arrow = if self.ascending {
            egui_phosphor::fill::CARET_UP
        } else {
            egui_phosphor::fill::CARET_DOWN
        };
        let cols = columns(rect);
        let font = FontId::proportional(13.0);
        let color = ui.visuals().strong_text_color();

        for (key, label, col_rect, align) in [
            (SortKey::Name, "Name", cols.name, Align2::LEFT_CENTER),
            (SortKey::Size, "Size", cols.size, Align2::RIGHT_CENTER),
            (
                SortKey::Modified,
                "Modified",
                cols.modified,
                Align2::LEFT_CENTER,
            ),
        ] {
            // Label in the text font; sort caret (if any) painted separately
            // in the icon font right after it.
            let label_w = painter
                .layout_no_wrap(label.to_string(), font.clone(), color)
                .size()
                .x;
            let (lx, arrow_x) = match align {
                Align2::RIGHT_CENTER => {
                    let right = col_rect.right() - PAD;
                    (right, right - label_w - 4.0)
                }
                _ => (col_rect.left() + PAD, col_rect.left() + PAD + label_w + 4.0),
            };
            painter.text(
                egui::pos2(lx, col_rect.center().y),
                align,
                label,
                font.clone(),
                color,
            );
            if self.sort == key {
                painter.text(
                    egui::pos2(arrow_x, col_rect.center().y),
                    Align2::LEFT_CENTER,
                    arrow,
                    crate::fonts::icon_font(13.0),
                    color,
                );
            }
            let resp = ui.interact(col_rect, ui.id().with(("hdr", label)), Sense::click());
            if resp.clicked() {
                self.sort_by(key);
            }
        }
        painter.hline(
            rect.x_range(),
            rect.bottom(),
            Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        );
    }
}

/// Apply a click to a tab's selection with Explorer semantics: plain click
/// selects one, Ctrl toggles, Shift extends from the anchor in visible
/// order.
fn apply_selection_click(
    selected: &mut HashSet<u32>,
    anchor: &mut Option<u32>,
    visible: &[u32],
    idx: u32,
    mods: egui::Modifiers,
) {
    if mods.ctrl {
        if !selected.remove(&idx) {
            selected.insert(idx);
        }
        *anchor = Some(idx);
    } else if mods.shift {
        if let (Some(a), Some(ra), Some(ri)) = (
            *anchor,
            anchor.and_then(|a| visible.iter().position(|&v| v == a)),
            visible.iter().position(|&v| v == idx),
        ) {
            let _ = a;
            selected.clear();
            for r in ra.min(ri)..=ra.max(ri) {
                selected.insert(visible[r]);
            }
        } else {
            selected.clear();
            selected.insert(idx);
            *anchor = Some(idx);
        }
    } else {
        selected.clear();
        selected.insert(idx);
        *anchor = Some(idx);
    }
}

pub struct SpikeApp {
    // Tabs: independent browse contexts sharing the app-level services.
    tabs: Vec<BrowseTab>,
    active_tab: usize,
    view_mode: ViewMode,

    // Sidebar contents, computed once at startup.
    quick_access: Vec<(String, PathBuf)>,
    drives: Vec<PathBuf>,
    /// One-shot focus request for a toolbar box (Ctrl+L / Ctrl+F / Ctrl+P).
    focus_target: Option<FocusTarget>,

    // Synthetic on-disk generation
    gen_rx: Option<Receiver<GenMsg>>,
    gen_progress: usize,

    // Whole-drive index + search. When the index exists and `drive_query`
    // is non-empty, the table shows drive-wide hits instead of the folder.
    index_rx: Option<Receiver<IndexMsg>>,
    /// RwLock because the USN tailer mutates the index in place while
    /// searches read it.
    index: Option<Arc<RwLock<FileIndex>>>,
    /// Probed once at startup: can this process use the MFT fast path?
    mft_ok: bool,
    index_progress: usize,
    index_info: String,
    /// Reports from the background thread persisting a fresh index.
    save_rx: Option<Receiver<String>>,
    /// Live journal tailer: notifications + its shutdown flag.
    tail_rx: Option<Receiver<TailMsg>>,
    tail_stop: Option<Arc<AtomicBool>>,
    tail_caught_up: bool,
    drive_query: String,
    /// Lowercased query the current hits belong to (search is
    /// case-insensitive, so "OV" -> "ov" must not rescan).
    computed_drive_query: String,
    /// Debounce state for full scans: (lowercased query, when it appeared).
    drive_pending: Option<(String, Instant)>,
    /// Recent query results, newest last. Backspacing restores from here
    /// instead of paying a full rescan.
    drive_cache: Vec<(String, Vec<u32>, usize)>,
    /// Async full-scan plumbing: results tagged with a generation so stale
    /// scans (superseded query, rebuilt index) are discarded on arrival.
    search_tx: Sender<(u64, String, SearchOutput)>,
    search_rx: Receiver<(u64, String, SearchOutput)>,
    search_gen: u64,
    search_inflight: Option<String>,
    drive_hits: Vec<u32>,
    drive_total: usize,
    drive_ms: f32,
    drive_selected: Option<u32>,

    // Shell icons/thumbnails, extracted off-thread and cached as textures.
    icons: IconCache,

    // Shell operations (open, context menus, verbs, recycle) on the STA
    // worker; events come back when something may have changed.
    shell_tx: Sender<ShellRequest>,
    shell_rx: Receiver<ShellEvent>,
    /// In-progress inline rename: (entry index, edit buffer, focused yet).
    rename_state: Option<(u32, String, bool)>,
    /// Cached shell background-menu enumeration for one folder, so the
    /// themed right-click menu shows the real (third-party) commands.
    bg_menu: Option<(PathBuf, Vec<MenuItem>)>,
    bg_menu_requested: Option<PathBuf>,
    /// Whether any text box (path/filter/search) had focus last frame.
    /// Gates the file-op shortcuts: clicking a row also takes egui focus
    /// (Sense::click is focusable), so "nothing focused" is the wrong test.
    text_input_focused: bool,

    // Perf
    update_ms: VecDeque<f32>,
    status: String,

    // Session log for post-hoc review.
    telem: Telemetry,
}

impl SpikeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::fonts::install(&cc.egui_ctx);
        crate::theme::apply(&cc.egui_ctx);
        let (search_tx, search_rx) = channel();
        let icons = IconCache::new(&cc.egui_ctx);
        let shell_repaint = cc.egui_ctx.clone();
        let (shell_tx, shell_rx) = spawn_shell_worker(move || shell_repaint.request_repaint());
        let view_mode = cc
            .storage
            .and_then(|s| s.get_string("fx_view_mode"))
            .map(|s| ViewMode::from_str(&s))
            .unwrap_or(ViewMode::List);
        // Sidebar contents: user folders that exist + mounted drives.
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
        let mut quick_access: Vec<(String, PathBuf)> = Vec::new();
        if let Some(home) = &home {
            for name in [
                "Desktop",
                "Downloads",
                "Documents",
                "Pictures",
                "Music",
                "Videos",
            ] {
                let p = home.join(name);
                if p.is_dir() {
                    quick_access.push((name.to_string(), p));
                }
            }
            quick_access.push(("Home".into(), home.clone()));
        }
        let drives = fx_platform::logical_drives();

        let mut app = Self {
            tabs: vec![BrowseTab::new()],
            active_tab: 0,
            view_mode,
            quick_access,
            drives,
            focus_target: None,
            gen_rx: None,
            gen_progress: 0,
            index_rx: Some(spawn_index_load()),
            index: None,
            mft_ok: fx_index::mft_available(),
            index_progress: 0,
            index_info: String::new(),
            save_rx: None,
            tail_rx: None,
            tail_stop: None,
            tail_caught_up: false,
            drive_query: String::new(),
            computed_drive_query: String::new(),
            drive_pending: None,
            drive_cache: Vec::new(),
            search_tx,
            search_rx,
            search_gen: 0,
            search_inflight: None,
            drive_hits: Vec::new(),
            drive_total: 0,
            drive_ms: 0.0,
            drive_selected: None,
            icons,
            shell_tx,
            shell_rx,
            rename_state: None,
            bg_menu: None,
            bg_menu_requested: None,
            text_input_focused: false,
            update_ms: VecDeque::with_capacity(240),
            status: String::new(),
            telem: Telemetry::new(),
        };
        app.navigate(PathBuf::from("C:\\"));
        app.status = format!("logging to {}", app.telem.path.display());
        app
    }

    fn synth_dir() -> PathBuf {
        std::env::temp_dir().join("fx-spike-100k")
    }

    fn tab(&self) -> &BrowseTab {
        &self.tabs[self.active_tab]
    }

    /// Navigate the active tab.
    fn navigate(&mut self, path: PathBuf) {
        self.tabs[self.active_tab].navigate(path, &mut self.telem);
        self.status.clear();
        self.rename_state = None;
    }

    fn new_tab(&mut self) {
        let dir = self
            .tab()
            .current_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("C:\\"));
        let mut tab = BrowseTab::new();
        tab.navigate(dir, &mut self.telem);
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        self.rename_state = None;
    }

    fn close_tab(&mut self, i: usize) {
        self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.new_tab();
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        self.rename_state = None;
    }

    /// Re-enumerate the current directory (after file operations).
    fn refresh(&mut self) {
        if let Some(dir) = self.tab().current_dir.clone() {
            self.navigate(dir);
        }
    }

    /// Full paths of everything selected in whichever view is active.
    fn selection_paths(&self) -> Vec<PathBuf> {
        if self.in_drive_search() {
            let (Some(lock), Some(hit)) = (self.index.as_ref(), self.drive_selected) else {
                return Vec::new();
            };
            vec![lock.read().unwrap().resolve_path(hit)]
        } else {
            let tab = self.tab();
            let Some(dir) = tab.current_dir.as_ref() else {
                return Vec::new();
            };
            // Return in visible order so batch operations feel predictable.
            tab.visible
                .iter()
                .filter(|idx| tab.selected.contains(idx))
                .map(|&idx| dir.join(&tab.entries[idx as usize].name))
                .collect()
        }
    }

    fn create_new_folder(&mut self) {
        let Some(dir) = self.tab().current_dir.clone() else {
            return;
        };
        let mut name = "New folder".to_string();
        let mut n = 2;
        while dir.join(&name).exists() {
            name = format!("New folder ({n})");
            n += 1;
        }
        match std::fs::create_dir(dir.join(&name)) {
            Ok(()) => {
                self.telem.log("shell", format!("new folder {name:?}"));
                self.refresh();
            }
            Err(e) => self.status = format!("new folder: {e}"),
        }
    }

    fn commit_rename(&mut self, idx: u32, new_name: &str) {
        let Some(dir) = self.tab().current_dir.clone() else {
            return;
        };
        let old = &self.tab().entries[idx as usize].name;
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name == old {
            return;
        }
        match std::fs::rename(dir.join(old), dir.join(new_name)) {
            Ok(()) => {
                self.telem
                    .log("shell", format!("rename {old:?} -> {new_name:?}"));
                self.refresh();
            }
            Err(e) => self.status = format!("rename: {e}"),
        }
    }

    fn navigate_up(&mut self) {
        if let Some(parent) = self.tab().current_dir.as_ref().and_then(|d| d.parent()) {
            self.navigate(parent.to_path_buf());
        }
    }

    fn load_ram_synthetic(&mut self) {
        let t = Instant::now();
        let tab = &mut self.tabs[self.active_tab];
        tab.replace_entries(synthetic_entries(RAM_COUNT));
        self.telem.log(
            "ram-load",
            format!(
                "{RAM_COUNT} entries built in {:.1} ms",
                t.elapsed().as_secs_f32() * 1000.0
            ),
        );
        let tab = &mut self.tabs[self.active_tab];
        tab.recompute_reason = Some("ram-load");
        tab.enum_elapsed = Some(t.elapsed());
        tab.first_batch_ms = None;
        tab.enum_started = None;
        tab.rx = None;
        tab.current_dir = None;
        tab.path_input = format!("<RAM: {RAM_COUNT} synthetic entries>");
        self.status.clear();
    }

    fn drain_channels(&mut self) {
        // Every tab's enumeration stream, including background tabs.
        for i in 0..self.tabs.len() {
            let tab = &mut self.tabs[i];
            tab.drain_enum(&mut self.telem, &mut self.status);
        }

        if let Some(rx) = &self.index_rx {
            let mut done = false;
            loop {
                let msg = match rx.try_recv() {
                    Ok(m) => m,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // Worker ended without a terminal message (e.g. the
                        // startup load found nothing after its Note). Treat
                        // as finished or "indexing..." shows forever.
                        done = true;
                        break;
                    }
                };
                match msg {
                    IndexMsg::Progress(n) => self.index_progress = n,
                    IndexMsg::Note(n) => {
                        self.telem.log("index-note", &n);
                        self.status = n;
                    }
                    IndexMsg::Done {
                        index,
                        elapsed,
                        backend,
                    } => {
                        // Wind down any tailer attached to the old index.
                        if let Some(stop) = self.tail_stop.take() {
                            stop.store(true, Ordering::Relaxed);
                        }
                        self.tail_rx = None;
                        self.tail_caught_up = false;

                        let (count, journal_id, next_usn) =
                            (index.len(), index.journal_id, index.next_usn);
                        let index = Arc::new(RwLock::new(index));
                        self.index_info =
                            format!("{count} files ({backend}, {:.2} s)", elapsed.as_secs_f32());
                        self.telem.log("index-done", &self.index_info);

                        // Every cached/derived search result refers to the
                        // OLD index's positions — drop it all, and bump the
                        // generation so in-flight scans are discarded too.
                        self.drive_cache.clear();
                        self.drive_hits.clear();
                        self.drive_total = 0;
                        self.computed_drive_query.clear();
                        self.drive_selected = None;
                        self.drive_pending = None;
                        self.search_inflight = None;
                        self.search_gen += 1;

                        if backend != "disk" {
                            // Persist the fresh build in the background.
                            let idx = index.clone();
                            let (tx, rx) = channel();
                            self.save_rx = Some(rx);
                            std::thread::spawn(move || {
                                let path = fx_index::persist::default_index_path(Path::new("C:\\"));
                                let t = Instant::now();
                                let msg = match fx_index::persist::save(&idx.read().unwrap(), &path)
                                {
                                    Ok(()) => format!(
                                        "index saved in {:.1} s ({})",
                                        t.elapsed().as_secs_f32(),
                                        path.display()
                                    ),
                                    Err(e) => format!("index save failed: {e}"),
                                };
                                let _ = tx.send(msg);
                            });
                        }

                        if self.mft_ok && journal_id != 0 {
                            // Tail the journal: the first pass replays
                            // everything since build/save (delta catch-up),
                            // then it polls to keep the index live.
                            let (rx, stop) = fx_index::spawn_tail(index.clone(), 'C');
                            self.tail_rx = Some(rx);
                            self.tail_stop = Some(stop);
                            if backend == "disk" {
                                self.status = "index loaded; live updates on".into();
                            }
                        } else if backend == "disk" {
                            self.status =
                                format!("index loaded ({})", index_freshness(journal_id, next_usn));
                            self.telem.log("index-fresh", &self.status);
                        }
                        self.index = Some(index);
                        done = true;
                    }
                    IndexMsg::Error(e) => {
                        self.telem.log("error", &e);
                        self.status = e;
                        done = true;
                    }
                }
            }
            if done {
                self.index_rx = None;
            }
        }

        // Async full-scan results. Collect first (try_recv borrows the
        // channel), then apply anything still current.
        let mut search_results = Vec::new();
        while let Ok(msg) = self.search_rx.try_recv() {
            search_results.push(msg);
        }
        for (gen, q, out) in search_results {
            if gen == self.search_gen {
                self.search_inflight = None;
                self.apply_drive_result(q, out.hits, out.total, out.ms, "full-async");
            }
        }

        // Live journal updates. Applied counts mean the index content moved
        // under us: cached/derived search results are stale (never unsafe —
        // entries only append or tombstone), so force a re-search.
        if self.tail_rx.is_some() {
            let mut applied = 0usize;
            let mut stopped: Option<String> = None;
            let mut disconnected = false;
            {
                let rx = self.tail_rx.as_ref().unwrap();
                loop {
                    match rx.try_recv() {
                        Ok(TailMsg::Applied { count }) => applied += count,
                        Ok(TailMsg::Stopped(e)) => {
                            stopped = Some(e);
                            break;
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
            }
            if applied > 0 {
                self.telem
                    .log("journal", format!("applied {applied} change(s)"));
                if !self.tail_caught_up {
                    self.tail_caught_up = true;
                    self.status = format!("caught up: {applied} changes replayed; live updates on");
                }
                self.drive_cache.clear();
                self.computed_drive_query.clear();
            }
            if let Some(e) = stopped {
                self.status = format!("live updates stopped: {e}");
                self.telem.log("journal", &self.status);
                self.tail_rx = None;
                self.tail_stop = None;
            } else if disconnected {
                self.tail_rx = None;
                self.tail_stop = None;
            }
        }

        // Shell operation outcomes.
        let mut shell_changed = false;
        while let Ok(ev) = self.shell_rx.try_recv() {
            match ev {
                ShellEvent::Changed => shell_changed = true,
                ShellEvent::Error(e) => {
                    self.telem.log("shell", &e);
                    self.status = e;
                }
                ShellEvent::BackgroundMenu { dir, items } => {
                    self.telem.log(
                        "bgmenu",
                        format!("{} items for {}", items.len(), dir.display()),
                    );
                    if self.bg_menu_requested.as_deref() == Some(dir.as_path()) {
                        self.bg_menu_requested = None;
                    }
                    self.bg_menu = Some((dir, items));
                }
            }
        }
        if shell_changed {
            self.telem.log("shell", "operation completed; refreshing");
            self.refresh();
        }

        if let Some(rx) = &self.save_rx {
            let mut done = false;
            let mut last = None;
            for msg in rx.try_iter() {
                last = Some(msg);
                done = true;
            }
            if let Some(msg) = last {
                self.telem.log("index-save", &msg);
                self.status = msg;
            }
            if done {
                self.save_rx = None;
            }
        }

        if let Some(rx) = &self.gen_rx {
            let mut done = false;
            let mut finished = false;
            for msg in rx.try_iter() {
                match msg {
                    GenMsg::Progress(n) => self.gen_progress = n,
                    GenMsg::Done { count, elapsed } => {
                        self.status =
                            format!("generated {count} files in {:.1} s", elapsed.as_secs_f32());
                        self.telem.log("gen-done", &self.status);
                        done = true;
                        finished = true;
                    }
                    GenMsg::Error(e) => {
                        self.telem.log("error", &e);
                        self.status = e;
                        done = true;
                    }
                }
            }
            if done {
                self.gen_rx = None;
                if finished {
                    self.navigate(Self::synth_dir());
                }
            }
        }
    }

    fn in_drive_search(&self) -> bool {
        self.index.is_some() && !self.drive_query.is_empty()
    }

    /// Install a search result as the current view and remember it in the
    /// query cache (so backspacing to this query is free later).
    fn apply_drive_result(
        &mut self,
        query_lower: String,
        hits: Vec<u32>,
        total: usize,
        ms: f32,
        mode: &str,
    ) {
        if !query_lower.is_empty() {
            self.drive_cache.retain(|(q, _, _)| *q != query_lower);
            self.drive_cache
                .push((query_lower.clone(), hits.clone(), total));
            if self.drive_cache.len() > 16 {
                self.drive_cache.remove(0);
            }
            self.telem.log(
                "drive-search",
                format!(
                    "query={query_lower:?} -> {total} matches ({} shown) in {ms:.2} ms [{mode}]",
                    hits.len(),
                ),
            );
        }
        self.computed_drive_query = query_lower;
        self.drive_hits = hits;
        self.drive_total = total;
        self.drive_ms = ms;
        self.drive_selected = None;
    }

    /// Run a full scan on a worker thread; the result comes back through
    /// `search_rx` tagged with a generation for staleness checks.
    fn dispatch_drive_search(
        &mut self,
        index: Arc<RwLock<FileIndex>>,
        query_lower: String,
        ctx: &egui::Context,
    ) {
        self.search_gen += 1;
        let gen = self.search_gen;
        self.search_inflight = Some(query_lower.clone());
        let tx = self.search_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let out = index.read().unwrap().search(&query_lower, DRIVE_HITS_CAP);
            if tx.send((gen, query_lower, out)).is_ok() {
                // The UI may be idle; make sure a frame runs to drain this.
                ctx.request_repaint();
            }
        });
    }

    /// Re-run the drive-wide search when its query changed.
    ///
    /// Cost ladder, cheapest first:
    ///   1. same query (case-insensitively): nothing to do
    ///   2. cached query (e.g. backspace): restore instantly
    ///   3. extension of a complete result set: rescan survivors, sub-ms
    ///   4. full scan: debounced 50 ms, then run on a worker thread so the
    ///      UI never blocks
    fn refresh_drive_search(&mut self, ctx: &egui::Context) {
        let ql = self.drive_query.to_lowercase();
        if ql == self.computed_drive_query {
            self.drive_pending = None;
            return;
        }
        let Some(index) = self.index.clone() else {
            self.computed_drive_query = ql;
            self.drive_hits.clear();
            self.drive_total = 0;
            return;
        };
        if ql.is_empty() {
            self.computed_drive_query.clear();
            self.drive_hits.clear();
            self.drive_total = 0;
            self.drive_pending = None;
            return;
        }

        if let Some(pos) = self.drive_cache.iter().position(|(q, _, _)| *q == ql) {
            let (q, hits, total) = self.drive_cache[pos].clone();
            self.drive_pending = None;
            self.apply_drive_result(q, hits, total, 0.0, "cached");
            return;
        }

        let incremental_ok = !self.computed_drive_query.is_empty()
            && self.drive_hits.len() == self.drive_total
            && ql.starts_with(&self.computed_drive_query);
        if incremental_ok {
            let out = index
                .read()
                .unwrap()
                .search_within(&ql, DRIVE_HITS_CAP, &self.drive_hits);
            self.drive_pending = None;
            self.apply_drive_result(ql, out.hits, out.total, out.ms, "incremental");
            return;
        }

        if self.search_inflight.as_deref() == Some(ql.as_str()) {
            return; // this exact scan is already running
        }
        match &self.drive_pending {
            Some((q, since)) if *q == ql => {
                if since.elapsed() >= DRIVE_DEBOUNCE {
                    self.drive_pending = None;
                    self.dispatch_drive_search(index, ql, ctx);
                } else {
                    ctx.request_repaint_after(DRIVE_DEBOUNCE - since.elapsed());
                }
            }
            _ => {
                self.drive_pending = Some((ql, Instant::now()));
                ctx.request_repaint_after(DRIVE_DEBOUNCE);
            }
        }
    }

    // ---- panels -----------------------------------------------------------

    /// Tab strip: proper tab-shaped chips (rounded top, close × inside the
    /// tab), + for a new tab.
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("tabs")
            .frame(
                egui::Frame::default()
                    .fill(theme::SURFACE_PANEL)
                    .inner_margin(egui::Margin {
                        left: 6,
                        right: 6,
                        top: 4,
                        bottom: 0,
                    }),
            )
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let mut activate: Option<usize> = None;
                    let mut close: Option<usize> = None;
                    let many = self.tabs.len() > 1;
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let (act, cls) = draw_tab(ui, i, &tab.title(), i == self.active_tab, many);
                        if act {
                            activate = Some(i);
                        }
                        if cls {
                            close = Some(i);
                        }
                    }
                    ui.add_space(2.0);
                    if ui
                        .add(egui::Button::new(icon(egui_phosphor::fill::PLUS)).frame(false))
                        .on_hover_text("New tab (Ctrl+T)")
                        .clicked()
                    {
                        self.new_tab();
                    }
                    if let Some(i) = activate {
                        self.active_tab = i;
                        self.rename_state = None;
                    }
                    if let Some(i) = close {
                        self.close_tab(i);
                    }
                });
            });
    }

    /// Empty-space behaviour for a browse view: left-click deselects,
    /// right-click opens fx's own themed menu, populated with the folder's
    /// real shell background commands (third-party tools included). New folder
    /// and Paste are implemented natively — the shell's own versions need a
    /// full folder-view site we don't provide — and shown at the top.
    fn background_menu(&mut self, bg: &egui::Response) {
        if bg.clicked() {
            let tab = &mut self.tabs[self.active_tab];
            tab.selected.clear();
            tab.select_anchor = None;
        }
        let Some(dir) = self.tabs[self.active_tab].current_dir.clone() else {
            return; // RAM view — no folder to act on
        };

        // Take the enumerated items for this dir if we have them; otherwise
        // request them once (the menu shows the native essentials meanwhile).
        let items: Option<Vec<MenuItem>> = match &self.bg_menu {
            Some((d, items)) if *d == dir => Some(items.clone()),
            _ => {
                if self.bg_menu_requested.as_deref() != Some(dir.as_path()) {
                    self.bg_menu_requested = Some(dir.clone());
                    let _ = self
                        .shell_tx
                        .send(ShellRequest::EnumBackground(dir.clone()));
                }
                None
            }
        };

        let mut action: Option<BgAction> = None;
        bg.context_menu(|ui| {
            ui.set_min_width(190.0);
            if ui.button("New folder").clicked() {
                action = Some(BgAction::NewFolder);
                ui.close();
            }
            if ui.button("Paste").clicked() {
                action = Some(BgAction::Paste);
                ui.close();
            }
            match &items {
                Some(items) => {
                    if items.iter().any(keep_bg_item) {
                        ui.separator();
                        render_shell_menu(ui, items, &mut action);
                    }
                }
                None => {
                    ui.separator();
                    ui.add_enabled(false, egui::Button::new("Loading commands…"));
                }
            }
            ui.separator();
            if ui.button("Refresh").clicked() {
                action = Some(BgAction::Refresh);
                ui.close();
            }
        });

        match action {
            Some(BgAction::NewFolder) => self.create_new_folder(),
            Some(BgAction::Paste) => {
                let _ = self.shell_tx.send(ShellRequest::PasteInto(dir));
            }
            Some(BgAction::Refresh) => self.refresh(),
            Some(BgAction::Shell(id)) => {
                let _ = self
                    .shell_tx
                    .send(ShellRequest::InvokeBackground { dir, id });
            }
            None => {}
        }
    }

    /// Quick access + drives down the left edge.
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(160.0)
            .size_range(120.0..=400.0)
            .show_inside(ui, |ui| {
                let mut go: Option<PathBuf> = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Justified layout makes entries fill the panel width
                        // — both so they read as real nav rows and so the
                        // panel occupies its full width (required for the
                        // resize separator to be draggable).
                        ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                            ui.add_space(6.0);
                            ui.strong("Quick access");
                            for (label, path) in &self.quick_access {
                                if ui.selectable_label(false, label).clicked() {
                                    go = Some(path.clone());
                                }
                            }
                            ui.add_space(8.0);
                            ui.strong("Drives");
                            for drive in &self.drives {
                                if ui
                                    .selectable_label(false, drive.display().to_string())
                                    .clicked()
                                {
                                    go = Some(drive.clone());
                                }
                            }
                        });
                    });
                if let Some(path) = go {
                    self.drive_query.clear();
                    self.navigate(path);
                }
            });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("toolbar").show_inside(ui, |ui| {
            let active = self.active_tab;
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Up").clicked() {
                    self.navigate_up();
                }
                let path_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.tabs[active].path_input)
                        .desired_width(ui.available_width() - 480.0)
                        .hint_text("path (Ctrl+L)"),
                );
                if self.focus_target == Some(FocusTarget::Path) {
                    path_edit.request_focus();
                    self.focus_target = None;
                }
                if path_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let p = PathBuf::from(self.tabs[active].path_input.trim());
                    self.navigate(p);
                }
                let filter_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.tabs[active].query)
                        .desired_width(220.0)
                        .hint_text("filter folder (Ctrl+F)"),
                );
                if self.focus_target == Some(FocusTarget::Filter) {
                    filter_edit.request_focus();
                    self.focus_target = None;
                }
                if filter_edit.changed() {
                    self.tabs[active].recompute_reason = Some("query");
                }
                let drive_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.drive_query)
                        .desired_width(ui.available_width() - 8.0)
                        .hint_text(if self.index.is_some() {
                            "search whole drive (Ctrl+P)"
                        } else {
                            "search drive (build index first)"
                        }),
                );
                if self.focus_target == Some(FocusTarget::DriveSearch) {
                    drive_edit.request_focus();
                    self.focus_target = None;
                }
                self.text_input_focused =
                    path_edit.has_focus() || filter_edit.has_focus() || drive_edit.has_focus();
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let synth = Self::synth_dir();
                if self.gen_rx.is_some() {
                    ui.label(format!("generating... {}", self.gen_progress));
                } else if ui.button("Generate 100k files").clicked() {
                    self.gen_progress = 0;
                    self.gen_rx = Some(spawn_generate(synth, SYNTH_COUNT));
                }
                if ui.button("500k (RAM)").clicked() {
                    self.load_ram_synthetic();
                }
                ui.separator();
                if ui.button("New folder").clicked() {
                    self.create_new_folder();
                }
                ui.separator();
                for (mode, label) in [
                    (ViewMode::List, "List"),
                    (ViewMode::LargeList, "Large"),
                    (ViewMode::Grid, "Grid"),
                ] {
                    if ui.selectable_label(self.view_mode == mode, label).clicked() {
                        self.view_mode = mode;
                    }
                }
                ui.separator();
                // Sort control that works in every view (grid has no
                // clickable header).
                let (cur_sort, asc) = (self.tabs[active].sort, self.tabs[active].ascending);
                let sort_label = |k: SortKey| match k {
                    SortKey::Name => "Name",
                    SortKey::Size => "Size",
                    SortKey::Modified => "Modified",
                };
                egui::ComboBox::from_id_salt("sortsel")
                    .selected_text(format!("Sort: {}", sort_label(cur_sort)))
                    .show_ui(ui, |ui| {
                        for key in [SortKey::Name, SortKey::Size, SortKey::Modified] {
                            if ui
                                .selectable_label(cur_sort == key, sort_label(key))
                                .clicked()
                                && cur_sort != key
                            {
                                let tab = &mut self.tabs[active];
                                tab.sort = key;
                                tab.ascending = key == SortKey::Name;
                                tab.recompute_reason = Some("sort");
                                tab.sort_dirty = true;
                            }
                        }
                    });
                let dir_glyph = if asc {
                    egui_phosphor::fill::SORT_ASCENDING
                } else {
                    egui_phosphor::fill::SORT_DESCENDING
                };
                if ui
                    .button(icon(dir_glyph))
                    .on_hover_text("Toggle sort direction")
                    .clicked()
                {
                    let tab = &mut self.tabs[active];
                    tab.ascending = !tab.ascending;
                    tab.recompute_reason = Some("sort");
                    tab.sort_dirty = true;
                }
                ui.separator();
                if self.index_rx.is_some() {
                    ui.label(format!("indexing... {}", self.index_progress));
                } else {
                    let label = if self.index.is_some() {
                        "Rebuild index".to_string()
                    } else if self.mft_ok {
                        "Build drive index (C:, fast MFT)".to_string()
                    } else {
                        "Build drive index (C:, slow walk)".to_string()
                    };
                    if ui.button(label).clicked() {
                        self.index_progress = 0;
                        self.telem.log("index-start", "C:\\");
                        self.index_rx = Some(fx_index::spawn_build(PathBuf::from("C:\\")));
                    }
                    // The MFT pass needs an elevated process; offer the
                    // relaunch instead of silently indexing 6x slower.
                    if !self.mft_ok
                        && ui
                            .button("Restart as admin (fast index)")
                            .on_hover_text(
                                "Reading the NTFS master file table (seconds instead of \
                                 minutes) requires opening the volume raw, which Windows \
                                 only allows for elevated processes.",
                            )
                            .clicked()
                    {
                        self.telem.log("elevate", "relaunching elevated");
                        if fx_index::relaunch_elevated() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        } else {
                            self.status = "elevation cancelled or failed".into();
                        }
                    }
                    if !self.index_info.is_empty() {
                        ui.label(&self.index_info);
                    }
                }
            });
            ui.add_space(4.0);
        });
    }

    fn perf_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("perf").show_inside(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let tab = &self.tabs[self.active_tab];
                let enum_txt = match (tab.enum_elapsed, tab.enum_started, &tab.rx) {
                    (Some(e), _, _) => format!("enum {:.1} ms", e.as_secs_f32() * 1000.0),
                    (None, Some(s), Some(_)) => {
                        format!("enum {:.0} ms...", s.elapsed().as_secs_f32() * 1000.0)
                    }
                    _ => "enum -".into(),
                };
                let first = tab
                    .first_batch_ms
                    .map(|ms| format!("first paint {ms:.1} ms"))
                    .unwrap_or_default();
                let (avg, worst) = frame_stats(&self.update_ms);
                // Left: status + index state (the bits a user cares about).
                if self.in_drive_search() {
                    ui.label(
                        egui::RichText::new(format!(
                            "drive: {} matches ({} shown) in {:.2} ms",
                            self.drive_total,
                            self.drive_hits.len(),
                            self.drive_ms,
                        ))
                        .color(theme::TEXT_SECONDARY),
                    );
                } else if !self.index_info.is_empty() {
                    let live = if self.tail_rx.is_some() { ", live" } else { "" };
                    ui.label(
                        egui::RichText::new(format!("index: {}{live}", self.index_info))
                            .color(theme::TEXT_SECONDARY),
                    );
                }
                if !self.status.is_empty() {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(&self.status).color(Color32::from_rgb(224, 184, 92)),
                    );
                }
                // Right: the dev perf HUD, muted so it recedes.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {} · {enum_txt} {first} · filter {:.2} · sort {:.2} · ui {avg:.2}/{worst:.2} ms · icons {}",
                            tab.visible.len(),
                            tab.entries.len(),
                            tab.filter_ms,
                            tab.sort_ms,
                            self.icons.len(),
                        ))
                        .small()
                        .color(theme::TEXT_MUTED),
                    );
                });
            });
            ui.add_space(2.0);
        });
    }

    /// Drive-wide search results: name + full parent path, in index order.
    /// Paths resolve lazily per visible row (a few map hits each).
    fn drive_table(&mut self, ui: &mut egui::Ui) {
        if self.view_mode == ViewMode::Grid {
            self.drive_grid_view(ui);
            return;
        }
        let (row_h, icon_px) = match self.view_mode {
            ViewMode::LargeList => (LARGE_ROW_H, 36.0f32),
            _ => (ROW_H, 16.0f32),
        };
        let panel_fill = theme::SURFACE_LIST;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(panel_fill))
            .show_inside(ui, |ui| {
                let width = ui.available_width();
                let name_w = (width * 0.35).clamp(180.0, 420.0);

                // Header (not sortable: results are in index order).
                let (rect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, theme::SURFACE_FAINT);
                let font = FontId::proportional(12.5);
                painter.text(
                    egui::pos2(rect.left() + PAD, rect.center().y),
                    Align2::LEFT_CENTER,
                    "Name",
                    font.clone(),
                    theme::TEXT_SECONDARY,
                );
                painter.text(
                    egui::pos2(rect.left() + name_w + PAD, rect.center().y),
                    Align2::LEFT_CENTER,
                    "Location",
                    font,
                    theme::TEXT_SECONDARY,
                );
                painter.hline(
                    rect.x_range(),
                    rect.bottom(),
                    Stroke::new(1.0, theme::BORDER),
                );

                ui.spacing_mut().item_spacing.y = 0.0;
                let mut navigate_to: Option<PathBuf> = None;
                let dir_color = theme::FOLDER_TINT;
                let text_color = theme::TEXT_PRIMARY;
                let weak_color = theme::TEXT_MUTED;
                let name_font = FontId::proportional(13.0);
                let path_font = FontId::proportional(12.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, self.drive_hits.len(), |ui, range| {
                        let Some(lock) = &self.index else { return };
                        let index = lock.read().unwrap();
                        for row in range {
                            let hit = self.drive_hits[row];
                            let (hit_name, hit_is_dir) = (index.name(hit), index.is_dir(hit));
                            let (rect, resp) =
                                ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }
                            let painter = ui.painter_at(rect);
                            paint_row_bg(
                                &painter,
                                rect,
                                self.drive_selected == Some(hit),
                                resp.hovered(),
                            );

                            let name_rect = Rect::from_min_max(
                                rect.min,
                                egui::pos2(rect.left() + name_w, rect.max.y),
                            );
                            let path_rect = Rect::from_min_max(
                                egui::pos2(rect.left() + name_w, rect.min.y),
                                rect.max,
                            );
                            let parent = index.resolve_parent(hit);
                            let mut name_x = name_rect.left() + PAD;
                            if let Some(tid) =
                                self.icons
                                    .get(hit_is_dir, &parent.join(hit_name), icon_px as u32)
                            {
                                let icon_rect = Rect::from_center_size(
                                    egui::pos2(name_x + icon_px / 2.0, rect.center().y),
                                    vec2(icon_px, icon_px),
                                );
                                painter.image(
                                    tid,
                                    icon_rect,
                                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            }
                            name_x += icon_px + 6.0;
                            ui.painter_at(name_rect).text(
                                egui::pos2(name_x, name_rect.center().y),
                                Align2::LEFT_CENTER,
                                hit_name,
                                name_font.clone(),
                                if hit_is_dir { dir_color } else { text_color },
                            );
                            ui.painter_at(path_rect).text(
                                egui::pos2(path_rect.left() + PAD, path_rect.center().y),
                                Align2::LEFT_CENTER,
                                parent.display().to_string(),
                                path_font.clone(),
                                weak_color,
                            );

                            if resp.double_clicked() {
                                if hit_is_dir {
                                    navigate_to = Some(index.resolve_path(hit));
                                } else {
                                    let _ = self
                                        .shell_tx
                                        .send(ShellRequest::Open(index.resolve_path(hit)));
                                }
                            } else if resp.secondary_clicked() {
                                self.drive_selected = Some(hit);
                                let _ = self
                                    .shell_tx
                                    .send(ShellRequest::ContextMenu(vec![index.resolve_path(hit)]));
                            } else if resp.clicked() {
                                self.drive_selected = Some(hit);
                            }
                        }
                    });

                if let Some(path) = navigate_to {
                    self.drive_query.clear();
                    self.navigate(path);
                }
            });
    }

    /// Grid layout for drive-search hits: same tiles as the browse grid,
    /// with the location shown as a hover tooltip (no column for it).
    fn drive_grid_view(&mut self, ui: &mut egui::Ui) {
        let panel_fill = theme::SURFACE_LIST;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(panel_fill))
            .show_inside(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let width = ui.available_width();
                let cols = ((width / GRID_TILE_W).floor() as usize).max(1);
                let n_rows = self.drive_hits.len().div_ceil(cols);
                let mut navigate_to: Option<PathBuf> = None;
                let dir_color = theme::FOLDER_TINT;
                let text_color = theme::TEXT_PRIMARY;
                let name_font = FontId::proportional(12.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, GRID_TILE_H, n_rows, |ui, range| {
                        let Some(lock) = &self.index else { return };
                        let index = lock.read().unwrap();
                        for grid_row in range {
                            let (row_rect, _) =
                                ui.allocate_exact_size(vec2(width, GRID_TILE_H), Sense::hover());
                            if !ui.is_rect_visible(row_rect) {
                                continue;
                            }
                            for col in 0..cols {
                                let i = grid_row * cols + col;
                                let Some(&hit) = self.drive_hits.get(i) else {
                                    break;
                                };
                                let (hit_name, hit_is_dir) = (index.name(hit), index.is_dir(hit));
                                let parent = index.resolve_parent(hit);
                                let tile = Rect::from_min_size(
                                    egui::pos2(
                                        row_rect.left() + col as f32 * GRID_TILE_W,
                                        row_rect.top(),
                                    ),
                                    vec2(GRID_TILE_W, GRID_TILE_H),
                                );
                                let resp = ui
                                    .interact(
                                        tile.shrink(3.0),
                                        ui.id().with(("dtile", i)),
                                        Sense::click(),
                                    )
                                    .on_hover_text(parent.join(hit_name).display().to_string());
                                let painter = ui.painter_at(tile);
                                if self.drive_selected == Some(hit) {
                                    painter.rect_filled(
                                        tile.shrink(3.0),
                                        6.0,
                                        ui.visuals().selection.bg_fill,
                                    );
                                } else if resp.hovered() {
                                    painter.rect_filled(
                                        tile.shrink(3.0),
                                        6.0,
                                        ui.visuals().widgets.hovered.weak_bg_fill,
                                    );
                                }

                                let icon_center = egui::pos2(
                                    tile.center().x,
                                    tile.top() + 10.0 + GRID_ICON / 2.0,
                                );
                                if let Some(tid) = self.icons.get(
                                    hit_is_dir,
                                    &parent.join(hit_name),
                                    GRID_ICON as u32,
                                ) {
                                    painter.image(
                                        tid,
                                        Rect::from_center_size(
                                            icon_center,
                                            vec2(GRID_ICON, GRID_ICON),
                                        ),
                                        Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        Color32::WHITE,
                                    );
                                }

                                let galley = painter.layout(
                                    hit_name.to_string(),
                                    name_font.clone(),
                                    if hit_is_dir { dir_color } else { text_color },
                                    GRID_TILE_W - 14.0,
                                );
                                let text_pos = egui::pos2(
                                    tile.center().x
                                        - (galley.size().x.min(GRID_TILE_W - 14.0)) / 2.0,
                                    tile.top() + 16.0 + GRID_ICON,
                                );
                                painter.galley(text_pos, galley, text_color);

                                if resp.double_clicked() {
                                    if hit_is_dir {
                                        navigate_to = Some(index.resolve_path(hit));
                                    } else {
                                        let _ = self
                                            .shell_tx
                                            .send(ShellRequest::Open(index.resolve_path(hit)));
                                    }
                                } else if resp.secondary_clicked() {
                                    self.drive_selected = Some(hit);
                                    let _ = self.shell_tx.send(ShellRequest::ContextMenu(vec![
                                        index.resolve_path(hit),
                                    ]));
                                } else if resp.clicked() {
                                    self.drive_selected = Some(hit);
                                }
                            }
                        }
                    });

                if let Some(path) = navigate_to {
                    self.drive_query.clear();
                    self.navigate(path);
                }
            });
    }

    /// Tile grid: thumbnail on top, wrapped name below. Virtualized the
    /// same way as the lists — `show_rows` over grid rows, so only visible
    /// tiles are laid out regardless of entry count.
    fn grid_view(&mut self, ui: &mut egui::Ui) {
        let panel_fill = theme::SURFACE_LIST;
        let active = self.active_tab;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(panel_fill))
            .show_inside(ui, |ui| {
                let bg = ui.interact(ui.max_rect(), ui.id().with("grid_bg"), Sense::click());
                ui.spacing_mut().item_spacing.y = 0.0;
                let width = ui.available_width();
                let mut navigate_to: Option<PathBuf> = None;
                let mut rename_commit: Option<(u32, String)> = None;
                let mut drop_req: Option<ShellRequest> = None;
                let dir_color = theme::FOLDER_TINT;
                let text_color = theme::TEXT_PRIMARY;
                let name_font = FontId::proportional(12.0);

                let BrowseTab {
                    entries,
                    visible,
                    selected,
                    select_anchor,
                    current_dir,
                    ..
                } = &mut self.tabs[active];
                let (entries, visible, current_dir) = (&*entries, &*visible, &*current_dir);
                let cols = ((width / GRID_TILE_W).floor() as usize).max(1);
                let n_rows = visible.len().div_ceil(cols);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .scroll_source(egui::scroll_area::ScrollSource {
                        drag: false,
                        ..egui::scroll_area::ScrollSource::ALL
                    })
                    .show_rows(ui, GRID_TILE_H, n_rows, |ui, range| {
                        for grid_row in range {
                            let (row_rect, _) =
                                ui.allocate_exact_size(vec2(width, GRID_TILE_H), Sense::hover());
                            if !ui.is_rect_visible(row_rect) {
                                continue;
                            }
                            for col in 0..cols {
                                let i = grid_row * cols + col;
                                let Some(&idx) = visible.get(i) else { break };
                                let entry = &entries[idx as usize];
                                let tile = Rect::from_min_size(
                                    egui::pos2(
                                        row_rect.left() + col as f32 * GRID_TILE_W,
                                        row_rect.top(),
                                    ),
                                    vec2(GRID_TILE_W, GRID_TILE_H),
                                );
                                let resp = ui.interact(
                                    tile.shrink(3.0),
                                    ui.id().with(("tile", i)),
                                    Sense::click_and_drag(),
                                );
                                let painter = ui.painter_at(tile);
                                if selected.contains(&idx) {
                                    painter.rect_filled(
                                        tile.shrink(3.0),
                                        6.0,
                                        theme::SELECTION_FILL,
                                    );
                                } else if resp.hovered() {
                                    painter.rect_filled(tile.shrink(3.0), 6.0, theme::ROW_HOVER);
                                }
                                if entry.is_dir && resp.dnd_hover_payload::<DragPaths>().is_some() {
                                    painter.rect_stroke(
                                        tile.shrink(3.0),
                                        6.0,
                                        Stroke::new(2.0, theme::ACCENT),
                                        egui::StrokeKind::Inside,
                                    );
                                }

                                let icon_center = egui::pos2(
                                    tile.center().x,
                                    tile.top() + 10.0 + GRID_ICON / 2.0,
                                );
                                if let Some(dir) = current_dir {
                                    if let Some(tid) = self.icons.get(
                                        entry.is_dir,
                                        &dir.join(&entry.name),
                                        GRID_ICON as u32,
                                    ) {
                                        painter.image(
                                            tid,
                                            Rect::from_center_size(
                                                icon_center,
                                                vec2(GRID_ICON, GRID_ICON),
                                            ),
                                            Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            Color32::WHITE,
                                        );
                                    }
                                }

                                let renaming = self
                                    .rename_state
                                    .as_ref()
                                    .is_some_and(|(ri, _, _)| *ri == idx);
                                if renaming {
                                    let edit_rect = Rect::from_min_max(
                                        egui::pos2(
                                            tile.left() + 5.0,
                                            tile.top() + 14.0 + GRID_ICON,
                                        ),
                                        egui::pos2(
                                            tile.right() - 5.0,
                                            tile.top() + 34.0 + GRID_ICON,
                                        ),
                                    );
                                    if let Some(commit) =
                                        rename_editor(&mut self.rename_state, ui, edit_rect)
                                    {
                                        rename_commit = Some(commit);
                                    }
                                } else {
                                    // Wrapped name under the icon, clipped to
                                    // the tile (at most ~2 lines fit).
                                    let galley = painter.layout(
                                        entry.name.clone(),
                                        name_font.clone(),
                                        if entry.is_dir { dir_color } else { text_color },
                                        GRID_TILE_W - 14.0,
                                    );
                                    let text_pos = egui::pos2(
                                        tile.center().x
                                            - (galley.size().x.min(GRID_TILE_W - 14.0)) / 2.0,
                                        tile.top() + 16.0 + GRID_ICON,
                                    );
                                    painter.galley(text_pos, galley, text_color);
                                }

                                if resp.drag_started() {
                                    if let Some(dir) = current_dir {
                                        let set = drag_set(dir, entries, visible, selected, idx);
                                        egui::DragAndDrop::set_payload(ui.ctx(), DragPaths(set));
                                    }
                                }
                                if entry.is_dir {
                                    if let Some(payload) = resp.dnd_release_payload::<DragPaths>() {
                                        if let Some(dir) = current_dir {
                                            let ctrl = ui.input(|i| i.modifiers.ctrl);
                                            drop_req = drop_request(
                                                payload.0.clone(),
                                                dir.join(&entry.name),
                                                ctrl,
                                            );
                                        }
                                    }
                                }

                                if resp.double_clicked() {
                                    if let Some(dir) = current_dir {
                                        if entry.is_dir {
                                            navigate_to = Some(dir.join(&entry.name));
                                        } else {
                                            let _ = self
                                                .shell_tx
                                                .send(ShellRequest::Open(dir.join(&entry.name)));
                                        }
                                    }
                                } else if resp.secondary_clicked() {
                                    if !selected.contains(&idx) {
                                        selected.clear();
                                        selected.insert(idx);
                                        *select_anchor = Some(idx);
                                    }
                                    if let Some(dir) = current_dir {
                                        let paths: Vec<PathBuf> = visible
                                            .iter()
                                            .filter(|v| selected.contains(v))
                                            .map(|&v| dir.join(&entries[v as usize].name))
                                            .collect();
                                        let _ =
                                            self.shell_tx.send(ShellRequest::ContextMenu(paths));
                                    }
                                } else if resp.clicked() {
                                    let mods = ui.input(|i| i.modifiers);
                                    apply_selection_click(
                                        selected,
                                        select_anchor,
                                        visible,
                                        idx,
                                        mods,
                                    );
                                }
                            }
                        }
                    });

                if let Some(req) = drop_req {
                    self.telem.log("dnd", "internal drop (grid)");
                    let _ = self.shell_tx.send(req);
                }
                self.background_menu(&bg);
                if let Some((idx, name)) = rename_commit {
                    self.commit_rename(idx, &name);
                }
                if let Some(path) = navigate_to {
                    self.navigate(path);
                }
            });
    }

    fn file_table(&mut self, ui: &mut egui::Ui) {
        if self.view_mode == ViewMode::Grid {
            self.grid_view(ui);
            return;
        }
        let (row_h, icon_px) = match self.view_mode {
            ViewMode::LargeList => (LARGE_ROW_H, 36.0f32),
            _ => (ROW_H, 16.0f32),
        };
        let panel_fill = theme::SURFACE_LIST;
        let active = self.active_tab;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(panel_fill))
            .show_inside(ui, |ui| {
                // Background catcher, registered first so the header and rows
                // (drawn after) sit on top for hit-testing. Left-click on the
                // empty area deselects; right-click opens the folder-background
                // menu (New, Paste, …).
                let bg = ui.interact(ui.max_rect(), ui.id().with("list_bg"), Sense::click());

                self.tabs[active].header(ui);
                ui.spacing_mut().item_spacing.y = 0.0;

                let mut navigate_to: Option<PathBuf> = None;
                let mut rename_commit: Option<(u32, String)> = None;
                let mut drop_req: Option<ShellRequest> = None;
                let dir_color = theme::FOLDER_TINT;
                let text_color = theme::TEXT_PRIMARY;
                let weak_color = theme::TEXT_MUTED;
                let name_font = FontId::proportional(13.0);
                let meta_font = FontId::monospace(11.5);

                // Split the tab's fields: rows read entries/visible while
                // click handling mutates the selection.
                let BrowseTab {
                    entries,
                    visible,
                    selected,
                    select_anchor,
                    current_dir,
                    ..
                } = &mut self.tabs[active];
                let (entries, visible, current_dir) = (&*entries, &*visible, &*current_dir);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    // Scroll by wheel/scrollbar only: a row drag must start an
                    // item drag, not scroll the list.
                    .scroll_source(egui::scroll_area::ScrollSource {
                        drag: false,
                        ..egui::scroll_area::ScrollSource::ALL
                    })
                    .show_rows(ui, row_h, visible.len(), |ui, range| {
                        let width = ui.available_width();
                        for row in range {
                            let idx = visible[row];
                            let entry = &entries[idx as usize];
                            let (rect, resp) =
                                ui.allocate_exact_size(vec2(width, row_h), Sense::click_and_drag());
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }

                            let painter = ui.painter_at(rect);
                            paint_row_bg(&painter, rect, selected.contains(&idx), resp.hovered());
                            // Folder highlighted as a drop target while a drag
                            // hovers it.
                            if entry.is_dir && resp.dnd_hover_payload::<DragPaths>().is_some() {
                                painter.rect_stroke(
                                    rect.shrink2(vec2(theme::SM, 1.0)),
                                    theme::RADIUS_SM,
                                    Stroke::new(2.0, theme::ACCENT),
                                    egui::StrokeKind::Inside,
                                );
                            }

                            let cols = columns(rect);
                            let mut name_x = cols.name.left() + PAD;
                            // Icons only where entries are real files (the
                            // RAM stress set has no paths to extract from).
                            if let Some(dir) = current_dir {
                                if let Some(tid) = self.icons.get(
                                    entry.is_dir,
                                    &dir.join(&entry.name),
                                    icon_px as u32,
                                ) {
                                    let icon_rect = Rect::from_center_size(
                                        egui::pos2(name_x + icon_px / 2.0, rect.center().y),
                                        vec2(icon_px, icon_px),
                                    );
                                    painter.image(
                                        tid,
                                        icon_rect,
                                        Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        Color32::WHITE,
                                    );
                                }
                                name_x += icon_px + 6.0;
                            }
                            let renaming = self
                                .rename_state
                                .as_ref()
                                .is_some_and(|(ri, _, _)| *ri == idx);
                            if renaming {
                                // Fixed-height editor centered in the row:
                                // filling a Large row makes a 40px text box.
                                let h = 20.0f32.min(rect.height() - 4.0);
                                let edit_rect = Rect::from_min_max(
                                    egui::pos2(name_x, rect.center().y - h / 2.0),
                                    egui::pos2(cols.name.right() - 4.0, rect.center().y + h / 2.0),
                                );
                                if let Some(commit) =
                                    rename_editor(&mut self.rename_state, ui, edit_rect)
                                {
                                    rename_commit = Some(commit);
                                }
                            } else {
                                let name_color = if entry.is_dir { dir_color } else { text_color };
                                ui.painter_at(cols.name).text(
                                    egui::pos2(name_x, cols.name.center().y),
                                    Align2::LEFT_CENTER,
                                    &entry.name,
                                    name_font.clone(),
                                    name_color,
                                );
                            }
                            if !entry.is_dir {
                                painter.text(
                                    egui::pos2(cols.size.right() - PAD, cols.size.center().y),
                                    Align2::RIGHT_CENTER,
                                    format_size(entry.size),
                                    meta_font.clone(),
                                    weak_color,
                                );
                            }
                            painter.text(
                                egui::pos2(cols.modified.left() + PAD, cols.modified.center().y),
                                Align2::LEFT_CENTER,
                                format_unix(entry.modified),
                                meta_font.clone(),
                                weak_color,
                            );

                            // Drag source: start carrying the drag set.
                            if resp.drag_started() {
                                if let Some(dir) = current_dir {
                                    let set = drag_set(dir, entries, visible, selected, idx);
                                    egui::DragAndDrop::set_payload(ui.ctx(), DragPaths(set));
                                }
                            }
                            // Drop target: a folder receiving a dragged set.
                            if entry.is_dir {
                                if let Some(payload) = resp.dnd_release_payload::<DragPaths>() {
                                    if let Some(dir) = current_dir {
                                        let ctrl = ui.input(|i| i.modifiers.ctrl);
                                        drop_req = drop_request(
                                            payload.0.clone(),
                                            dir.join(&entry.name),
                                            ctrl,
                                        );
                                    }
                                }
                            }

                            if resp.double_clicked() {
                                if let Some(dir) = current_dir {
                                    if entry.is_dir {
                                        navigate_to = Some(dir.join(&entry.name));
                                    } else {
                                        let _ = self
                                            .shell_tx
                                            .send(ShellRequest::Open(dir.join(&entry.name)));
                                    }
                                }
                            } else if resp.secondary_clicked() {
                                // Right-click outside the selection retargets
                                // it; inside, the menu covers all selected.
                                if !selected.contains(&idx) {
                                    selected.clear();
                                    selected.insert(idx);
                                    *select_anchor = Some(idx);
                                }
                                if let Some(dir) = current_dir {
                                    let paths: Vec<PathBuf> = visible
                                        .iter()
                                        .filter(|v| selected.contains(v))
                                        .map(|&v| dir.join(&entries[v as usize].name))
                                        .collect();
                                    let _ = self.shell_tx.send(ShellRequest::ContextMenu(paths));
                                }
                            } else if resp.clicked() {
                                let mods = ui.input(|i| i.modifiers);
                                apply_selection_click(selected, select_anchor, visible, idx, mods);
                            }
                        }
                    });

                if let Some(req) = drop_req {
                    self.telem.log("dnd", "internal drop");
                    let _ = self.shell_tx.send(req);
                }
                self.background_menu(&bg);
                if let Some((idx, name)) = rename_commit {
                    self.commit_rename(idx, &name);
                }
                if let Some(path) = navigate_to {
                    self.navigate(path);
                }
            });
    }
}

/// One line describing whether a disk-loaded index still matches the
/// volume, for the cases where the tailer can't run and catch up instead.
fn index_freshness(journal_id: u64, next_usn: i64) -> &'static str {
    if journal_id == 0 {
        return "freshness unknown: no journal info saved";
    }
    match fx_index::journal_position('C') {
        Some((jid, next)) if jid == journal_id && next == next_usn => "up to date",
        Some((jid, _)) if jid == journal_id => "drive changed since save; Rebuild to refresh",
        Some(_) => "journal reset; Rebuild recommended",
        None => "freshness unknown without admin",
    }
}

/// Try to load a previously saved index from disk on a worker thread,
/// reporting through the same channel shape the builders use.
fn spawn_index_load() -> Receiver<IndexMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let path = fx_index::persist::default_index_path(Path::new("C:\\"));
        let t = Instant::now();
        match fx_index::persist::load(&path) {
            Ok(index) => {
                let _ = tx.send(IndexMsg::Done {
                    index,
                    elapsed: t.elapsed(),
                    backend: "disk",
                });
            }
            Err(e) => {
                // First run (or deleted index): normal, not an error.
                let _ = tx.send(IndexMsg::Note(format!(
                    "no saved index yet; Build drive index once and it will auto-load next launch [{e}]"
                )));
            }
        }
    });
    rx
}

/// Draw the inline rename editor into `rect`. Returns Some((idx, name)) on
/// commit (Enter or click-away); clears the state on Escape. A free
/// function so callers can hold entry borrows while invoking it.
fn rename_editor(
    state: &mut Option<(u32, String, bool)>,
    ui: &mut egui::Ui,
    rect: Rect,
) -> Option<(u32, String)> {
    let (_, buf, focused) = state.as_mut().unwrap();
    let edit = ui.put(rect, egui::TextEdit::singleline(buf));
    if !*focused {
        edit.request_focus();
        *focused = true;
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        *state = None;
        None
    } else if edit.lost_focus() {
        let (ri, buf, _) = state.take().unwrap();
        Some((ri, buf))
    } else {
        None
    }
}

struct Columns {
    name: Rect,
    size: Rect,
    modified: Rect,
}

fn columns(row: Rect) -> Columns {
    let modified_left = row.right() - MODIFIED_W;
    let size_left = modified_left - SIZE_W;
    Columns {
        name: Rect::from_min_max(row.min, egui::pos2(size_left, row.max.y)),
        size: Rect::from_min_max(
            egui::pos2(size_left, row.min.y),
            egui::pos2(modified_left, row.max.y),
        ),
        modified: Rect::from_min_max(egui::pos2(modified_left, row.min.y), row.max),
    }
}

/// Draw one browser tab: a rounded-top chip with the title and (when more
/// than one tab is open) a close × inside it. The active tab is filled with
/// the content surface + an accent top-bar so it reads as attached to the
/// pane below. Returns (activate, close).
fn draw_tab(
    ui: &mut egui::Ui,
    idx: usize,
    title: &str,
    active: bool,
    closable: bool,
) -> (bool, bool) {
    let font = FontId::proportional(13.0);
    let pad = 10.0;
    let x_w = if closable { 18.0 } else { 0.0 };
    let text_w = ui
        .painter()
        .layout_no_wrap(title.to_owned(), font.clone(), theme::TEXT_PRIMARY)
        .size()
        .x
        .min(150.0);
    let w = pad + text_w + x_w + 6.0;
    let h = 28.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let painter = ui.painter_at(rect);

    let radius = egui::CornerRadius {
        nw: 6,
        ne: 6,
        sw: 0,
        se: 0,
    };
    let bg = if active {
        theme::SURFACE_LIST
    } else if resp.hovered() {
        theme::ROW_HOVER
    } else {
        theme::SURFACE_FAINT
    };
    painter.rect_filled(rect, radius, bg);
    if active {
        painter.rect_filled(
            Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), rect.top() + 2.0)),
            egui::CornerRadius {
                nw: 6,
                ne: 6,
                sw: 0,
                se: 0,
            },
            theme::ACCENT,
        );
    }

    let text_color = if active {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SECONDARY
    };
    let clip = Rect::from_min_size(egui::pos2(rect.left() + pad, rect.top()), vec2(text_w, h));
    ui.painter_at(clip).text(
        egui::pos2(rect.left() + pad, rect.center().y),
        Align2::LEFT_CENTER,
        title,
        font,
        text_color,
    );

    let mut close = false;
    let mut over_x = false;
    if closable {
        let cx = rect.right() - pad * 0.5 - x_w * 0.5;
        let x_rect = Rect::from_center_size(egui::pos2(cx, rect.center().y), vec2(x_w, x_w));
        let x_resp = ui.interact(x_rect, ui.id().with(("tabx", idx)), Sense::click());
        over_x = x_resp.hovered();
        if over_x {
            painter.rect_filled(x_rect, 4.0, theme::HOVER);
        }
        painter.text(
            x_rect.center(),
            Align2::CENTER_CENTER,
            egui_phosphor::bold::X,
            crate::fonts::icon_font_bold(11.0),
            if over_x {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_MUTED
            },
        );
        close = x_resp.clicked();
    }
    // Activating must not fire when the click landed on the × hit-area.
    (resp.clicked() && !over_x, close)
}

/// Payload carried during an internal drag: the full paths being dragged.
/// (egui requires the payload be `Any + Send + Sync`.)
#[derive(Clone)]
struct DragPaths(Vec<PathBuf>);

/// A background-menu choice, collected during rendering and applied after
/// (so the render closure never mutates `self`).
enum BgAction {
    NewFolder,
    Paste,
    Refresh,
    Shell(u32),
}

/// Whether an enumerated shell item belongs in our menu. We drop the ones we
/// handle natively (paste and the New submenu) and the folder-view built-ins
/// that can't be invoked without a hosted view (View / Sort by / Group by /
/// undo), leaving third-party tools and other real commands. Label matching
/// is English-only for now — a localization pass is future work.
fn keep_bg_item(item: &MenuItem) -> bool {
    match item {
        MenuItem::Separator => true,
        MenuItem::Command { verb, .. } => !matches!(
            verb.as_deref(),
            Some("paste") | Some("pastelink") | Some("undo") | Some("redo")
        ),
        MenuItem::Submenu { label, .. } => {
            let l = label.trim().to_ascii_lowercase();
            !matches!(
                l.as_str(),
                "new" | "view" | "sort by" | "group by" | "arrange icons by"
            )
        }
    }
}

/// Render enumerated shell items into the themed menu, coalescing runs of
/// separators and dropping leading/trailing ones.
fn render_shell_menu(ui: &mut egui::Ui, items: &[MenuItem], action: &mut Option<BgAction>) {
    let kept: Vec<&MenuItem> = items.iter().filter(|i| keep_bg_item(i)).collect();
    let mut pending_sep = false;
    let mut emitted = false;
    for item in kept {
        match item {
            MenuItem::Separator => {
                pending_sep = emitted;
            }
            MenuItem::Command {
                id, label, enabled, ..
            } => {
                if pending_sep {
                    ui.separator();
                    pending_sep = false;
                }
                if ui.add_enabled(*enabled, egui::Button::new(label)).clicked() {
                    *action = Some(BgAction::Shell(*id));
                    ui.close();
                }
                emitted = true;
            }
            MenuItem::Submenu { label, items } => {
                if pending_sep {
                    ui.separator();
                    pending_sep = false;
                }
                ui.menu_button(label, |ui| render_shell_menu(ui, items, action));
                emitted = true;
            }
        }
    }
}

/// The paths to drag when a row is grabbed: the whole selection if the
/// grabbed row is part of it, otherwise just that row.
fn drag_set(
    dir: &Path,
    entries: &[Entry],
    visible: &[u32],
    selected: &HashSet<u32>,
    idx: u32,
) -> Vec<PathBuf> {
    if selected.contains(&idx) && selected.len() > 1 {
        visible
            .iter()
            .filter(|v| selected.contains(v))
            .map(|&v| dir.join(&entries[v as usize].name))
            .collect()
    } else {
        vec![dir.join(&entries[idx as usize].name)]
    }
}

/// Turn a completed internal drop into a shell request: move by default,
/// copy while Ctrl is held (Explorer's convention). Sources that equal the
/// destination folder are dropped — you can't move a folder into itself.
fn drop_request(sources: Vec<PathBuf>, dest: PathBuf, ctrl: bool) -> Option<ShellRequest> {
    let sources: Vec<PathBuf> = sources.into_iter().filter(|s| *s != dest).collect();
    if sources.is_empty() {
        return None;
    }
    Some(if ctrl {
        ShellRequest::CopyInto { sources, dest }
    } else {
        ShellRequest::MoveInto { sources, dest }
    })
}

/// Selection / hover background for a list row: an inset, rounded fill
/// (a subtle "pill") rather than an edge-to-edge block — reads cleaner and
/// modern. No zebra striping.
fn paint_row_bg(painter: &egui::Painter, rect: Rect, selected: bool, hovered: bool) {
    let inset = rect.shrink2(vec2(theme::SM, 1.0));
    if selected {
        painter.rect_filled(inset, theme::RADIUS_SM, theme::SELECTION_FILL);
    } else if hovered {
        painter.rect_filled(inset, theme::RADIUS_SM, theme::ROW_HOVER);
    }
}

fn frame_stats(samples: &VecDeque<f32>) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let sum: f32 = samples.iter().sum();
    let worst = samples.iter().cloned().fold(0.0, f32::max);
    (sum / samples.len() as f32, worst)
}

impl eframe::App for SpikeApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("fx_view_mode", self.view_mode.as_str().to_string());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();

        // View, tab, and focus shortcuts (always active).
        let mut new_tab = false;
        let mut close_tab = false;
        let mut cycle_tab = false;
        ui.ctx().input_mut(|i| {
            use egui::{Key, Modifiers};
            for (key, mode) in [
                (Key::Num1, ViewMode::List),
                (Key::Num2, ViewMode::LargeList),
                (Key::Num3, ViewMode::Grid),
            ] {
                if i.consume_key(Modifiers::CTRL, key) {
                    self.view_mode = mode;
                }
            }
            new_tab = i.consume_key(Modifiers::CTRL, Key::T);
            close_tab = i.consume_key(Modifiers::CTRL, Key::W);
            cycle_tab = i.consume_key(Modifiers::CTRL, Key::Tab);
            if i.consume_key(Modifiers::CTRL, Key::L) {
                self.focus_target = Some(FocusTarget::Path);
            }
            if i.consume_key(Modifiers::CTRL, Key::F) {
                self.focus_target = Some(FocusTarget::Filter);
            }
            if i.consume_key(Modifiers::CTRL, Key::P) {
                self.focus_target = Some(FocusTarget::DriveSearch);
            }
        });
        if new_tab {
            self.new_tab();
        }
        if close_tab {
            self.close_tab(self.active_tab);
        }
        if cycle_tab && self.tabs.len() > 1 {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
            self.rename_state = None;
        }

        // File-operation shortcuts — only when no text field has focus
        // (based on last frame's toolbar state; rows taking click-focus
        // must not block these).
        if !self.text_input_focused && self.rename_state.is_none() {
            let sel = self.selection_paths();
            enum Act {
                Recycle,
                Rename,
                Refresh,
                Verb(&'static str),
                Paste,
                NewFolder,
                Open,
            }
            let mut act = None;
            ui.ctx().input_mut(|i| {
                use egui::{Key, Modifiers};
                if i.consume_key(Modifiers::NONE, Key::Delete) {
                    act = Some(Act::Recycle);
                } else if i.consume_key(Modifiers::NONE, Key::F2) {
                    act = Some(Act::Rename);
                } else if i.consume_key(Modifiers::NONE, Key::F5) {
                    act = Some(Act::Refresh);
                } else if i.consume_key(Modifiers::CTRL, Key::C) {
                    act = Some(Act::Verb("copy"));
                } else if i.consume_key(Modifiers::CTRL, Key::X) {
                    act = Some(Act::Verb("cut"));
                } else if i.consume_key(Modifiers::CTRL, Key::V) {
                    act = Some(Act::Paste);
                } else if i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::N) {
                    act = Some(Act::NewFolder);
                } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                    act = Some(Act::Open);
                }
            });
            if act.is_some() {
                let name = match &act {
                    Some(Act::Recycle) => "delete",
                    Some(Act::Rename) => "rename",
                    Some(Act::Refresh) => "refresh",
                    Some(Act::Verb(v)) => v,
                    Some(Act::Paste) => "paste",
                    Some(Act::NewFolder) => "new-folder",
                    Some(Act::Open) => "open",
                    None => unreachable!(),
                };
                self.telem
                    .log("key", format!("{name} ({} selected)", sel.len()));
            }
            match act {
                Some(Act::Recycle) => {
                    if !sel.is_empty() {
                        let _ = self.shell_tx.send(ShellRequest::Recycle(sel));
                    }
                }
                Some(Act::Rename) => {
                    if !self.in_drive_search() && self.tab().selected.len() == 1 {
                        let idx = *self.tab().selected.iter().next().unwrap();
                        let name = self.tab().entries[idx as usize].name.clone();
                        self.rename_state = Some((idx, name, false));
                    }
                }
                Some(Act::Refresh) => self.refresh(),
                Some(Act::Verb(v)) => {
                    if !sel.is_empty() {
                        let _ = self.shell_tx.send(ShellRequest::InvokeVerb(sel, v));
                    }
                }
                Some(Act::Paste) => {
                    if !self.in_drive_search() {
                        if let Some(dir) = self.tab().current_dir.clone() {
                            let _ = self.shell_tx.send(ShellRequest::PasteInto(dir));
                        }
                    }
                }
                Some(Act::NewFolder) => self.create_new_folder(),
                Some(Act::Open) => {
                    // Single dir -> enter it; otherwise open every selected file.
                    if sel.len() == 1 && sel[0].is_dir() {
                        self.drive_query.clear();
                        self.navigate(sel.into_iter().next().unwrap());
                    } else {
                        for p in sel {
                            if !p.is_dir() {
                                let _ = self.shell_tx.send(ShellRequest::Open(p));
                            }
                        }
                    }
                }
                None => {}
            }
        }

        let ctx = ui.ctx().clone();

        // Files dropped onto the window from another app (Explorer, etc.):
        // copy them into the active folder. Cross-app drops default to copy.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            if let Some(dir) = self.tab().current_dir.clone() {
                self.telem.log(
                    "drop",
                    format!("{} item(s) -> {}", dropped.len(), dir.display()),
                );
                let _ = self.shell_tx.send(ShellRequest::CopyInto {
                    sources: dropped,
                    dest: dir,
                });
            }
        }

        self.drain_channels();
        self.icons.begin_frame(&ctx);
        self.tabs[self.active_tab].refresh_view(&mut self.telem);
        self.refresh_drive_search(&ctx);

        self.toolbar(ui);
        self.tab_bar(ui);
        self.perf_bar(ui);
        self.sidebar(ui);
        if self.in_drive_search() {
            self.drive_table(ui);
        } else {
            self.file_table(ui);
        }

        // Hint overlay while files are hovering over the window.
        let hovering = ctx.input(|i| i.raw.hovered_files.len());
        if hovering > 0 && !self.in_drive_search() {
            if let Some(dir) = self.tab().current_dir.clone() {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                egui::Area::new(egui::Id::new("drop_hint"))
                    .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -48.0))
                    .order(egui::Order::Foreground)
                    .show(&ctx, |ui| {
                        egui::Frame::popup(&ctx.global_style()).show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Copy {hovering} item(s) into \u{201c}{name}\u{201d}"
                                ))
                                .color(theme::TEXT_PRIMARY),
                            );
                        });
                    });
            }
        }

        // Keep repainting while background work streams in. (Async searches
        // don't need this: their worker requests a repaint on completion.)
        if self.tabs.iter().any(|t| t.rx.is_some())
            || self.gen_rx.is_some()
            || self.index_rx.is_some()
            || self.save_rx.is_some()
        {
            ui.ctx().request_repaint();
        } else if self.icons.missing_any() {
            // Visible rows are waiting on icon extraction; tick slowly so
            // dropped-request retries fire even when the user is idle.
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        if self.update_ms.len() >= 240 {
            self.update_ms.pop_front();
        }
        let update_ms = t0.elapsed().as_secs_f32() * 1000.0;
        self.update_ms.push_back(update_ms);
        // Report whichever view is actually on screen, so spike/heartbeat
        // lines aren't labeled with the hidden browse view during search.
        let (shown, total) = if self.in_drive_search() {
            let total = self.index.as_ref().map_or(0, |i| i.read().unwrap().len());
            (self.drive_hits.len(), total)
        } else {
            let tab = self.tab();
            (tab.visible.len(), tab.entries.len())
        };
        self.telem.frame(update_ms, shown, total, &self.update_ms);
    }
}
