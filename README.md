# fx

A fast, lean file explorer for Windows, built in Rust on egui/eframe.
Dual-licensed MIT OR Apache-2.0. The performance bar: people should ask
"how is it this fast?"

- **Instant whole-drive search** — reads the NTFS master file table
  directly and stays live via the USN journal (the Everything technique):
  ~4.5M files indexed in seconds, loaded from disk in ~0.5 s on launch,
  searched in tens of milliseconds.
- **Never blocks a frame** — enumeration, indexing, search, icon
  extraction, and shell operations all run off the UI thread. Views are
  virtualized, so a 100k-file folder scrolls at full frame rate.
- **Real file management** — native shell context menus (third-party
  entries included), Recycle Bin deletes, rename, clipboard interop with
  Explorer, tabs, multi-select, a sidebar, and list/large/grid views with
  shell icons and thumbnails.

> Cross-platform is a non-goal for now: the differentiating work (MFT
> indexing, shell integration) is Windows-specific, though the core is
> kept behind platform seams.

## Status: milestone 7 — product layer

The spike grew a product shell: tabs (Ctrl+T/W/Tab; each tab owns its own
directory, entries, sort, filter, and selection), a sidebar with quick
access + drives, Explorer-style multi-select (Ctrl/Shift+click) feeding
batch operations (recycle, copy/cut, context menus over the whole
selection), a sort dropdown that works in grid view, and focus shortcuts
(Ctrl+L path, Ctrl+F filter, Ctrl+P drive search). Structurally,
navigation state moved into `BrowseTab`; the app struct now owns tabs +
shared services (index, icons, shell worker, telemetry). Deferred:
drag-and-drop (OLE), per-tab drive search.

## Milestone 6 — shell integration & file operations

A dedicated STA shell thread (fx-platform::shell) hosts the REAL Explorer
context menu — `IContextMenu` with a hidden owner window forwarding menu
messages, so third-party entries (7-Zip, Git, Send To, Open With) work.
Double-click opens files with their default app; Del recycles via
`IFileOperation` (undo-able); F2 renames inline; F5 refreshes;
Ctrl+C/X/V invoke shell verbs (full clipboard interop with Explorer);
Ctrl+Shift+N / toolbar button creates folders. Works in every view,
including drive-search results.

## Milestone 5 — shell icons + thumbnails

`fx-platform` is born: a COM (STA) worker thread extracts the same icons
and thumbnails Explorer shows via `IShellItemImageFactory::GetImage`,
returning raw RGBA over channels (no UI dependency). The app caches them
as egui textures with LRU eviction — extension-keyed for common files,
path-keyed for things with per-file art (image thumbnails, .exe, .lnk) —
in both the browse table and drive-search results. Rows render instantly
and icons pop in a frame later; scrolling cost is unchanged.

## Milestone 4 — live index (USN journal tailing) + index v2

The index is now mutable and permanently fresh: when elevated, a tailer
thread reads the NTFS USN change journal once a second and applies
creates/deletes/renames to the shared index in place (RwLock; deletes
tombstone so search hits stay valid). Its first pass replays everything
since the last save — launch, catch up, live within a second. Storage is
v2: all names packed in one byte arena (no per-name allocation, ~3x
faster builds, roughly half the memory), id lookup via sorted-array
binary search + overlay map for new files. Saves compact tombstones out.

## Milestone 3 — persistent index, cached + async search

The drive index now persists to `%LOCALAPPDATA%\fx-spike\index-C.bin`
(atomic write, ~40 MB per million entries) and auto-loads on launch in
well under a second, with USN-journal-position freshness reporting when
elevated. Search has a four-tier cost ladder: unchanged query (free) ->
cached query, e.g. backspace (free) -> incremental narrowing (sub-ms) ->
debounced full scan on a worker thread (UI never blocks). Next: live USN
journal tailing so the index tracks changes while running.

## Milestone 2 — whole-drive instant search

`fx-index` indexes every filename on the volume the way Everything does:
one sequential pass over the NTFS master file table (`FSCTL_ENUM_USN_DATA`)
when elevated (~4.5M files in ~27 s), falling back to a parallel directory
walk without elevation. Searches scan all names across all cores
(case-insensitive substring, ~25-40 ms over 4.8M files) and paths resolve
lazily by climbing parent ids. In the app: "Build drive index", then type
in the drive-search box; double-click a result to jump to it.

Headless: `cargo run --release -p fx-app -- --index C:\` (run elevated for
the MFT fast path). Next: USN journal tailing for live index updates,
async search off the UI thread, packed name storage.

## Milestone 1 — the speed spike

Proves the core premise before any product code: open a 100k-file
directory instantly, scroll at full frame rate, fuzzy-filter-as-you-type
in single-digit milliseconds. Rendering cost is proportional to *visible*
rows only (virtualized `show_rows` + direct painter text), and the UI
thread never touches the disk — enumeration streams in from a worker
thread in batches.

## Workspace

| Crate | Role |
|---|---|
| `fx-core` | UI-free engine: streaming enumeration, fuzzy filter, sort. Zero dependencies. |
| `fx-app` | eframe binary: virtualized table, perf bar, headless bench mode |

Planned next: `fx-index` (NTFS MFT + USN journal instant whole-drive
search), `fx-platform` (shell integration: context menus, icons,
IFileOperation, cloud placeholders).

## Run

```sh
cargo run --release -p fx-app
```

The perf bar along the bottom shows live numbers: enumeration time,
time-to-first-paint, filter/sort cost per keystroke, and UI frame cost.
Toolbar buttons load test workloads, including generating a real
100k-file directory in `%TEMP%\fx-spike-100k` and a 500k-entry in-memory
stress set.

Headless measurement (no window):

```sh
cargo run --release -p fx-app -- --gen  %TEMP%\fx-spike-100k 100000
cargo run --release -p fx-app -- --bench C:\Windows\System32
cargo run --release -p fx-app -- --bench %TEMP%\fx-spike-100k
```
