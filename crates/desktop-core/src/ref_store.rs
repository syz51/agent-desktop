use std::collections::{HashMap, VecDeque};

/// Default number of recent snapshots retained per backend ref store so inspecting another app
/// does not immediately invalidate earlier `@eN` element refs.
pub const DEFAULT_REF_SNAPSHOT_CAPACITY: usize = 3;

struct SnapshotSlot<T> {
    snapshot_id: String,
    refs: HashMap<String, T>,
}

/// Bounded LRU store retaining `@eN` element refs across the most recent `N` snapshots.
///
/// Each snapshot allocates a monotonically increasing contiguous range of `@eN` IDs via
/// [`Self::alloc_ref_start`], so multi-app snapshots do not shadow each other's `@eN` refs while
/// both remain in the LRU window. Looking up a ref promotes its owning snapshot to
/// most-recently-used; inserting beyond capacity evicts the least-recently-used snapshot.
pub struct SnapshotRefLru<T> {
    capacity: usize,
    next_ref_seq: usize,
    /// Ordered from least-recently-used (front) to most-recently-used (back).
    slots: VecDeque<SnapshotSlot<T>>,
}

impl<T> Default for SnapshotRefLru<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SnapshotRefLru<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_REF_SNAPSHOT_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            next_ref_seq: 1,
            slots: VecDeque::with_capacity(cap),
        }
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Reserve a contiguous range of `count.max(1)` `@eN` ref numbers and return its starting
    /// index (`1` for the very first snapshot).
    pub fn alloc_ref_start(&mut self, count: usize) -> usize {
        let n = count.max(1);
        let start = self.next_ref_seq;
        self.next_ref_seq = self.next_ref_seq.saturating_add(n);
        start
    }

    /// Insert a snapshot's refs as most-recently-used, evicting the least-recently-used snapshot
    /// if capacity would be exceeded.
    pub fn insert(&mut self, snapshot_id: impl Into<String>, refs: HashMap<String, T>) {
        let snapshot_id = snapshot_id.into();
        if let Some(pos) = self.slots.iter().position(|s| s.snapshot_id == snapshot_id) {
            self.slots.remove(pos);
        }
        while self.slots.len() >= self.capacity {
            self.slots.pop_front();
        }
        self.slots.push_back(SnapshotSlot { snapshot_id, refs });
    }

    /// Look up an element ref (`@eN` or optional `<snapshot_id>:@eN`) across retained snapshots,
    /// searching most-recently-used first and promoting the matched snapshot to most-recently-used.
    pub fn get(&mut self, ref_id: &str) -> Option<&T> {
        if let Some((sid, bare)) = ref_id.split_once(':') {
            let pos = self
                .slots
                .iter()
                .rposition(|s| s.snapshot_id == sid && s.refs.contains_key(bare))?;
            if pos + 1 != self.slots.len()
                && let Some(slot) = self.slots.remove(pos)
            {
                self.slots.push_back(slot);
            }
            return self.slots.back().and_then(|s| s.refs.get(bare));
        }

        let pos = self
            .slots
            .iter()
            .rposition(|s| s.refs.contains_key(ref_id))?;
        if pos + 1 != self.slots.len()
            && let Some(slot) = self.slots.remove(pos)
        {
            self.slots.push_back(slot);
        }
        self.slots.back().and_then(|s| s.refs.get(ref_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_map(k: &str, v: &str) -> HashMap<String, String> {
        HashMap::from([(k.to_string(), v.to_string())])
    }

    #[test]
    fn retains_recent_snapshots_and_evicts_lru() {
        let mut lru: SnapshotRefLru<String> = SnapshotRefLru::with_capacity(3);
        assert_eq!(lru.capacity(), 3);
        assert!(lru.is_empty());

        let s1_start = lru.alloc_ref_start(2);
        assert_eq!(s1_start, 1);
        lru.insert(
            "s1",
            HashMap::from([
                ("@e1".into(), "Notes:window".into()),
                ("@e2".into(), "Notes:save".into()),
            ]),
        );

        let s2_start = lru.alloc_ref_start(2);
        assert_eq!(s2_start, 3);
        lru.insert(
            "s2",
            HashMap::from([
                ("@e3".into(), "Calc:window".into()),
                ("@e4".into(), "Calc:7".into()),
            ]),
        );

        // Inspecting App B does not invalidate App A's refs.
        assert_eq!(lru.get("@e2").map(String::as_str), Some("Notes:save"));
        assert_eq!(lru.get("@e4").map(String::as_str), Some("Calc:7"));

        let s3_start = lru.alloc_ref_start(1);
        assert_eq!(s3_start, 5);
        lru.insert("s3", single_map("@e5", "Safari:url"));
        assert_eq!(lru.len(), 3);

        // Touch s1 so s2 becomes the least-recently-used slot.
        assert_eq!(lru.get("@e1").map(String::as_str), Some("Notes:window"));

        let s4_start = lru.alloc_ref_start(1);
        assert_eq!(s4_start, 6);
        lru.insert("s4", single_map("@e6", "Terminal:prompt"));
        assert_eq!(lru.len(), 3);

        // s2 was LRU and must be evicted; s1, s3, s4 remain live.
        assert_eq!(lru.get("@e3"), None);
        assert_eq!(lru.get("@e4"), None);
        assert_eq!(lru.get("@e2").map(String::as_str), Some("Notes:save"));
        assert_eq!(lru.get("@e5").map(String::as_str), Some("Safari:url"));
        assert_eq!(lru.get("@e6").map(String::as_str), Some("Terminal:prompt"));
    }

    #[test]
    fn supports_snapshot_qualified_lookup_and_prefers_newer_on_overlap() {
        let mut lru: SnapshotRefLru<String> = SnapshotRefLru::with_capacity(2);
        lru.insert("s1", single_map("@e1", "AppA"));
        lru.insert("s2", single_map("@e1", "AppB"));

        // Plain lookup resolves most recent snapshot first.
        assert_eq!(lru.get("@e1").map(String::as_str), Some("AppB"));
        // Snapshot-qualified lookup targets a specific snapshot.
        assert_eq!(lru.get("s1:@e1").map(String::as_str), Some("AppA"));
        assert_eq!(lru.get("s2:@e1").map(String::as_str), Some("AppB"));
        assert_eq!(lru.get("s9:@e1"), None);
    }

    #[test]
    fn zero_capacity_clamps_to_one_and_duplicate_snapshot_id_replaces_in_place() {
        let mut lru: SnapshotRefLru<String> = SnapshotRefLru::with_capacity(0);
        assert_eq!(lru.capacity(), 1);
        lru.insert("s1", single_map("@e1", "old"));
        lru.insert("s1", single_map("@e1", "new"));
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.get("@e1").map(String::as_str), Some("new"));
    }
}
