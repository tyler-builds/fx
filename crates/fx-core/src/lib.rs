//! fx-core — the UI-free engine of the file explorer.
//!
//! Milestone-1 scope (the speed spike): streaming directory enumeration,
//! fuzzy filtering, and column sorting, all measurable. The cardinal rule of
//! the whole project lives here: **the UI thread never touches the disk.**
//! Enumeration runs on worker threads and streams batches over a channel;
//! the UI drains whatever has arrived each frame.

mod entry;
mod enumerate;
mod filter;
mod synth;
mod timefmt;

pub use entry::Entry;
pub use enumerate::{spawn_enumerate, Batch};
pub use filter::{filter_sorted, fuzzy_score, sort_indices, FilterOutput, SortKey, SortOutput};
pub use synth::{spawn_generate, synthetic_entries, GenMsg};
pub use timefmt::{format_size, format_unix};
