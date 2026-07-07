use crate::entry::Entry;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

/// A full sort pass over all entries. Recomputed only when the entry set or
/// the sort key/direction changes — never per keystroke.
pub struct SortOutput {
    /// Indices into the entries slice, in display order.
    pub indices: Vec<u32>,
    pub sort_ms: f32,
}

/// One filter pass. Selection from an already-sorted index preserves order,
/// so filtering never pays for sorting.
pub struct FilterOutput {
    /// Indices into the entries slice, in display order.
    pub visible: Vec<u32>,
    pub filter_ms: f32,
    /// True when only the previous result set was rescanned (query extended).
    pub incremental: bool,
}

/// fzf-style subsequence match of `query` against `name`, both lowercase.
/// Returns a score (higher = better) or None if the query doesn't match.
///
/// Scoring favors, in order: contiguous substring matches, matches starting
/// earlier in the name, and shorter names. Good enough to feel right in the
/// spike; a real ranking pass (word-boundary bonuses etc.) comes later.
pub fn fuzzy_score(query: &str, name: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    // Contiguous substring: strong score, earlier position wins.
    if let Some(pos) = name.find(query) {
        return Some(10_000 - pos as i32 - name.len() as i32);
    }
    // Subsequence: every query char appears in order.
    let mut chars = name.char_indices();
    let mut first_hit = 0usize;
    let mut is_first = true;
    for qc in query.chars() {
        let (idx, _) = chars.find(|&(_, nc)| nc == qc)?;
        if is_first {
            first_hit = idx;
            is_first = false;
        }
    }
    Some(1_000 - first_hit as i32 - name.len() as i32)
}

/// Sort all entry indices by `key`. This is the "master index": the display
/// order for every possible filter result. A few ms at 100k, ~150+ ms at
/// 500k — which is exactly why it must not run per keystroke.
pub fn sort_indices(entries: &[Entry], key: SortKey, ascending: bool) -> SortOutput {
    let t0 = Instant::now();
    let mut indices: Vec<u32> = (0..entries.len() as u32).collect();
    indices.sort_unstable_by(|&a, &b| {
        let (ea, eb) = (&entries[a as usize], &entries[b as usize]);
        // Directories always group before files, regardless of sort.
        eb.is_dir
            .cmp(&ea.is_dir)
            .then_with(|| {
                let ord = match key {
                    SortKey::Name => ea.name_lower.cmp(&eb.name_lower),
                    SortKey::Size => ea.size.cmp(&eb.size),
                    SortKey::Modified => ea.modified.cmp(&eb.modified),
                };
                if ascending {
                    ord
                } else {
                    ord.reverse()
                }
            })
            // Stable tiebreak so equal keys don't shuffle between passes.
            .then_with(|| ea.name_lower.cmp(&eb.name_lower))
    });
    let sort_ms = t0.elapsed().as_secs_f32() * 1000.0;
    SortOutput { indices, sort_ms }
}

/// Select the entries matching `query_lower` out of `sorted`, preserving its
/// order.
///
/// `prev` is the previous (query, result) pair, if still valid for the same
/// entries + sort. When the new query merely extends the old one, every
/// match must be a subset of the old matches, so only that (usually far
/// smaller) set is rescanned — the fzf trick that keeps keystroke cost
/// shrinking as the query narrows instead of staying O(all entries).
pub fn filter_sorted(
    entries: &[Entry],
    sorted: &[u32],
    query_lower: &str,
    prev: Option<(&str, &[u32])>,
) -> FilterOutput {
    let t0 = Instant::now();
    if query_lower.is_empty() {
        return FilterOutput {
            visible: sorted.to_vec(),
            filter_ms: t0.elapsed().as_secs_f32() * 1000.0,
            incremental: false,
        };
    }
    let (source, incremental) = match prev {
        Some((pq, pv)) if !pq.is_empty() && query_lower.starts_with(pq) => (pv, true),
        _ => (sorted, false),
    };
    let visible: Vec<u32> = source
        .iter()
        .copied()
        .filter(|&i| fuzzy_score(query_lower, &entries[i as usize].name_lower).is_some())
        .collect();
    FilterOutput {
        visible,
        filter_ms: t0.elapsed().as_secs_f32() * 1000.0,
        incremental,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_beats_subsequence() {
        let sub = fuzzy_score("net", "network.dll").unwrap();
        let seq = fuzzy_score("net", "notepad_extra_thing.exe").unwrap();
        assert!(sub > seq);
    }

    #[test]
    fn earlier_substring_wins() {
        let early = fuzzy_score("dll", "dllhost.exe").unwrap();
        let late = fuzzy_score("dll", "somelib.dll").unwrap();
        assert!(early > late);
    }

    #[test]
    fn non_match_is_none() {
        assert!(fuzzy_score("xyz", "abc").is_none());
        // Order matters for subsequences.
        assert!(fuzzy_score("ba", "abc").is_none());
    }

    #[test]
    fn empty_query_matches_everything() {
        assert!(fuzzy_score("", "anything").is_some());
    }

    #[test]
    fn dirs_sort_first_and_keys_apply() {
        let entries = vec![
            Entry::new("zeta.txt".into(), 10, 5, false),
            Entry::new("Alpha".into(), 0, 9, true),
            Entry::new("beta.txt".into(), 99, 1, false),
        ];
        let out = sort_indices(&entries, SortKey::Name, true);
        let names: Vec<&str> = out
            .indices
            .iter()
            .map(|&i| entries[i as usize].name.as_str())
            .collect();
        assert_eq!(names, ["Alpha", "beta.txt", "zeta.txt"]);

        let out = sort_indices(&entries, SortKey::Size, false);
        let names: Vec<&str> = out
            .indices
            .iter()
            .map(|&i| entries[i as usize].name.as_str())
            .collect();
        assert_eq!(names, ["Alpha", "beta.txt", "zeta.txt"]);
    }

    #[test]
    fn filter_narrows_and_preserves_sort_order() {
        let entries = vec![
            Entry::new("kernel32.dll".into(), 1, 0, false),
            Entry::new("notes.md".into(), 1, 0, false),
            Entry::new("akern.txt".into(), 1, 0, false),
        ];
        let sorted = sort_indices(&entries, SortKey::Name, true).indices;
        let out = filter_sorted(&entries, &sorted, "krn", None);
        let names: Vec<&str> = out
            .visible
            .iter()
            .map(|&i| entries[i as usize].name.as_str())
            .collect();
        // Both subsequence-match "krn"; display order stays name-sorted.
        assert_eq!(names, ["akern.txt", "kernel32.dll"]);
        assert!(!out.incremental);
    }

    #[test]
    fn incremental_filter_matches_full_scan() {
        let entries: Vec<Entry> = (0..500)
            .map(|i| Entry::new(format!("report_{i:03}.txt"), i, 0, false))
            .collect();
        let sorted = sort_indices(&entries, SortKey::Name, true).indices;

        let prev = filter_sorted(&entries, &sorted, "report_1", None);
        let inc = filter_sorted(
            &entries,
            &sorted,
            "report_12",
            Some(("report_1", &prev.visible)),
        );
        let full = filter_sorted(&entries, &sorted, "report_12", None);

        assert!(inc.incremental);
        assert_eq!(inc.visible, full.visible);
        // Narrower query strictly shrinks the result set (fuzzy match keeps
        // subsequence hits like report_102, so it's not just prefix names).
        assert!(!inc.visible.is_empty() && inc.visible.len() < prev.visible.len());

        // A shortened (backspaced) query must NOT take the incremental path.
        let back = filter_sorted(
            &entries,
            &sorted,
            "report",
            Some(("report_1", &prev.visible)),
        );
        assert!(!back.incremental);
        assert_eq!(back.visible.len(), 500);
    }
}
