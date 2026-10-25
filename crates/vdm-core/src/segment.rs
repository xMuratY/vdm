//! Segment table for a multi-connection download.
//!
//! A download of `total` bytes is split into segments that tile `[0, total)`.
//! Each segment is worked on by at most one worker at a time. An idle worker
//! takes an unfinished segment or, when none is left, splits the active
//! segment with the most bytes remaining (dynamic splitting).
//!
//! `reserved` covers bytes a worker has buffered but not yet written. Splits
//! happen after `reserved`, so two workers never write the same bytes.

use std::sync::{Mutex, MutexGuard};

use vdm_proto::SegmentProgress;

/// Index into the segment list. Segments are never removed, so ids are stable.
pub(crate) type SegmentId = usize;

pub(crate) struct SegmentTable {
    total: u64,
    min_split: u64,
    segs: Mutex<Vec<Seg>>,
}

/// One segment. Invariants: `start <= pos <= reserved <= end` and `start < end`.
#[derive(Debug, Clone, Copy)]
struct Seg {
    start: u64,
    /// Exclusive. Shrinks when the segment is split.
    end: u64,
    /// Next byte to be written.
    pos: u64,
    /// Bytes handed to the worker so far, written or buffered.
    reserved: u64,
    /// Whether a worker currently owns the segment.
    active: bool,
}

impl Seg {
    fn new(start: u64, end: u64) -> Self {
        Self { start, end, pos: start, reserved: start, active: false }
    }
}

impl SegmentTable {
    /// Splits `[0, total)` into up to `connections` equal segments, each at least
    /// `min_split` bytes where possible. The last segment takes the remainder.
    pub(crate) fn new(total: u64, connections: usize, min_split: u64) -> Self {
        let min_split = min_split.max(1);
        let connections = connections.max(1);
        let mut segs = Vec::new();
        if total > 0 {
            // `by_size <= total`, so every segment has at least one byte.
            let by_size = (total / min_split).max(1);
            let n = u64::try_from(connections).unwrap_or(u64::MAX).min(by_size);
            let size = total / n;
            for i in 0..n {
                let start = i * size;
                let end = if i + 1 == n { total } else { start + size };
                segs.push(Seg::new(start, end));
            }
        }
        Self { total, min_split, segs: Mutex::new(segs) }
    }

    /// Rebuilds a table from saved progress. Returns `None` if the segments do
    /// not tile `[0, total)` or any segment is malformed. All segments come back
    /// idle, with `reserved` set to `pos`.
    pub(crate) fn from_snapshot(
        total: u64,
        min_split: u64,
        segs: &[SegmentProgress],
    ) -> Option<Self> {
        let mut out = Vec::with_capacity(segs.len());
        // Each segment must start where the previous one ended. This also
        // rules out unsorted and overlapping input.
        let mut expect = 0;
        for s in segs {
            if s.start != expect || s.start >= s.end || s.pos < s.start || s.pos > s.end {
                return None;
            }
            out.push(Seg { pos: s.pos, reserved: s.pos, ..Seg::new(s.start, s.end) });
            expect = s.end;
        }
        // An empty list tiles `[0, 0)` only.
        if expect != total {
            return None;
        }
        Some(Self { total, min_split: min_split.max(1), segs: Mutex::new(out) })
    }

    /// Current progress of every segment, sorted by start. `pos` is the
    /// committed position, not `reserved`.
    pub(crate) fn snapshot(&self) -> Vec<SegmentProgress> {
        let mut out: Vec<SegmentProgress> = self
            .lock()
            .iter()
            .map(|s| SegmentProgress { start: s.start, end: s.end, pos: s.pos })
            .collect();
        out.sort_by_key(|s| s.start);
        out
    }

    /// Segments a connection is working on.
    pub(crate) fn active(&self) -> usize {
        self.lock().iter().filter(|s| s.active).count()
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Bytes written so far.
    pub(crate) fn downloaded(&self) -> u64 {
        self.lock().iter().map(|s| s.pos - s.start).sum()
    }

    /// True when every byte has been written.
    pub(crate) fn is_complete(&self) -> bool {
        self.lock().iter().all(|s| s.pos == s.end)
    }

    /// Gives a worker a segment: an idle unfinished one (lowest start first), or
    /// a split of the active segment with the most bytes left. Returns `None`
    /// when there is nothing to do.
    pub(crate) fn claim(&self) -> Option<SegmentId> {
        let mut segs = self.lock();
        let idle = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.active && s.pos < s.end)
            .min_by_key(|(_, s)| s.start)
            .map(|(i, _)| i);
        if let Some(id) = idle {
            segs[id].active = true;
            return Some(id);
        }

        // Dynamic split: take the active segment with the most bytes not yet reserved.
        let (victim, remaining) = segs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.active)
            .map(|(i, s)| (i, s.end - s.reserved))
            .max_by_key(|&(_, r)| r)?;
        if remaining < self.min_split.saturating_mul(2) {
            return None;
        }
        // `remaining >= 2`, so the split point lies strictly inside the range
        // and after everything already reserved.
        let old_end = segs[victim].end;
        let split_at = segs[victim].reserved + remaining / 2;
        segs[victim].end = split_at;
        segs.push(Seg { active: true, ..Seg::new(split_at, old_end) });
        Some(segs.len() - 1)
    }

    /// Current `(pos, end)` of a segment. `end` can shrink after a split.
    pub(crate) fn range(&self, id: SegmentId) -> (u64, u64) {
        let segs = self.lock();
        (segs[id].pos, segs[id].end)
    }

    /// Reserves up to `len` more bytes for the worker. Returns how many were
    /// granted; 0 means the segment is fully reserved.
    pub(crate) fn reserve(&self, id: SegmentId, len: u64) -> u64 {
        let mut segs = self.lock();
        let s = &mut segs[id];
        let allowed = len.min(s.end - s.reserved);
        s.reserved += allowed;
        allowed
    }

    /// Marks `n` reserved bytes as written to disk.
    pub(crate) fn commit(&self, id: SegmentId, n: u64) {
        let mut segs = self.lock();
        let s = &mut segs[id];
        debug_assert!(s.pos + n <= s.reserved, "commit beyond reserved bytes");
        s.pos += n;
    }

    /// The worker stopped. Bytes reserved but not committed are dropped, and the
    /// segment becomes claimable again.
    pub(crate) fn release(&self, id: SegmentId) {
        let mut segs = self.lock();
        let s = &mut segs[id];
        s.reserved = s.pos;
        s.active = false;
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Seg>> {
        self.segs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const MIB: u64 = 1 << 20;

    fn sp(start: u64, end: u64, pos: u64) -> SegmentProgress {
        SegmentProgress { start, end, pos }
    }

    /// Asserts that the segments tile `[0, total)` and each one is well formed.
    fn check_invariants(table: &SegmentTable) -> Vec<SegmentProgress> {
        let snap = table.snapshot();
        let mut expect = 0;
        for s in &snap {
            assert_eq!(s.start, expect, "gap or overlap in {snap:?}");
            assert!(s.start < s.end, "empty segment in {snap:?}");
            assert!(s.start <= s.pos && s.pos <= s.end, "pos out of range in {snap:?}");
            expect = s.end;
        }
        assert_eq!(expect, table.total());
        snap
    }

    #[test]
    fn new_zero_total_has_no_segments() {
        let t = SegmentTable::new(0, 8, MIB);
        assert!(t.snapshot().is_empty());
        assert!(t.is_complete());
        assert_eq!(t.downloaded(), 0);
        assert_eq!(t.claim(), None);
    }

    #[test]
    fn new_small_file_is_one_segment() {
        let t = SegmentTable::new(500, 8, MIB);
        assert_eq!(t.snapshot(), vec![sp(0, 500, 0)]);
    }

    #[test]
    fn new_clamps_zero_arguments() {
        assert_eq!(SegmentTable::new(10, 0, 0).snapshot(), vec![sp(0, 10, 0)]);
        assert_eq!(SegmentTable::new(10, 100, 1).snapshot().len(), 10);
    }

    #[test]
    fn new_splits_evenly_and_last_takes_remainder() {
        let total = 100 * MIB + 7;
        let t = SegmentTable::new(total, 8, MIB);
        let snap = check_invariants(&t);
        assert_eq!(snap.len(), 8);
        let size = 100 * MIB / 8;
        for (i, s) in snap.iter().enumerate() {
            assert_eq!(s.start, i as u64 * size);
            assert_eq!(s.pos, s.start);
        }
        assert_eq!(snap[0].end - snap[0].start, size);
        assert_eq!(snap[7].end, total);
        assert_eq!(snap[7].end - snap[7].start, size + 7);
    }

    #[test]
    fn claim_takes_idle_segments_lowest_start_first() {
        let t = SegmentTable::new(4000, 4, 1);
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.claim(), Some(1));
        // A released segment is idle again and has the lowest start.
        t.release(0);
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.claim(), Some(2));
        assert_eq!(t.claim(), Some(3));
    }

    #[test]
    fn claim_splits_largest_active_remaining_at_midpoint() {
        let t = SegmentTable::new(1000, 2, 10); // [0,500) [500,1000)
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.claim(), Some(1));
        assert_eq!(t.reserve(0, 100), 100); // seg 0 now has 400 left, seg 1 has 500

        // Seg 1 has the most left: split at 500 + 500/2.
        assert_eq!(t.claim(), Some(2));
        assert_eq!(t.range(1), (500, 750));
        assert_eq!(t.range(2), (750, 1000));
        assert_eq!(
            check_invariants(&t),
            vec![sp(0, 500, 0), sp(500, 750, 500), sp(750, 1000, 750)]
        );

        // Seg 0 now has the most left (400 vs 250 and 250): split at 100 + 400/2.
        assert_eq!(t.claim(), Some(3));
        assert_eq!(t.range(0), (0, 300));
        assert_eq!(t.range(3), (300, 500));
        check_invariants(&t);
    }

    #[test]
    fn claim_refuses_to_split_below_two_min_split() {
        // 100 bytes, min_split 51: 2 * 51 > 100, so no split.
        let t = SegmentTable::new(100, 1, 51);
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.claim(), None);

        // Exactly 2 * min_split is splittable.
        let t = SegmentTable::new(100, 1, 50);
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.claim(), Some(1));
        assert_eq!(t.snapshot(), vec![sp(0, 50, 0), sp(50, 100, 50)]);
    }

    #[test]
    fn split_respects_reserved_bytes() {
        let t = SegmentTable::new(1000, 1, 10);
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.reserve(0, 300), 300);
        // 700 left after reserved=300, so the split is at 300 + 350.
        assert_eq!(t.claim(), Some(1));
        assert_eq!(t.snapshot(), vec![sp(0, 650, 0), sp(650, 1000, 650)]);
    }

    #[test]
    fn reserve_clamps_to_end_and_returns_zero_when_done() {
        let t = SegmentTable::new(100, 1, 1);
        let id = t.claim().unwrap();
        assert_eq!(t.reserve(id, 60), 60);
        assert_eq!(t.reserve(id, 60), 40);
        assert_eq!(t.reserve(id, 1), 0);
        t.commit(id, 100);
        let (pos, end) = t.range(id);
        assert!(pos >= end);
        assert!(t.is_complete());
        t.release(id);
        assert_eq!(t.claim(), None);
    }

    #[test]
    fn reserve_after_split_is_clamped_to_new_end() {
        let t = SegmentTable::new(1000, 1, 10);
        let id = t.claim().unwrap();
        assert_eq!(t.reserve(id, 300), 300);
        assert!(t.claim().is_some());
        assert_eq!(t.range(id), (0, 650));
        assert_eq!(t.reserve(id, 1000), 350);
        assert_eq!(t.reserve(id, 1), 0);
    }

    #[test]
    fn commit_and_release_drop_uncommitted_bytes() {
        let t = SegmentTable::new(100, 1, 1);
        let id = t.claim().unwrap();
        assert_eq!(t.reserve(id, 40), 40);
        t.commit(id, 40);
        assert_eq!(t.range(id), (40, 100));
        assert_eq!(t.downloaded(), 40);

        assert_eq!(t.reserve(id, 20), 20); // buffered, not written
        t.release(id);
        assert_eq!(t.claim(), Some(0));
        assert_eq!(t.reserve(0, 100), 60);
    }

    #[test]
    fn from_snapshot_round_trips() {
        let t = SegmentTable::new(1000, 1, 10);
        let a = t.claim().unwrap();
        assert_eq!(t.reserve(a, 300), 300);
        t.commit(a, 200);
        assert_eq!(t.claim(), Some(1)); // split at 650
        let snap = t.snapshot();
        assert_eq!(snap, vec![sp(0, 650, 200), sp(650, 1000, 650)]);

        let back = SegmentTable::from_snapshot(1000, 10, &snap).unwrap();
        assert_eq!(back.snapshot(), snap);
        assert_eq!(back.downloaded(), t.downloaded());
        // Rebuilt segments are idle, and reserved starts at pos.
        assert_eq!(back.claim(), Some(0));
        assert_eq!(back.reserve(0, 1000), 450);
    }

    #[test]
    fn from_snapshot_rejects_invalid_layouts() {
        let ok = [sp(0, 100, 0), sp(100, 200, 150)];
        assert!(SegmentTable::from_snapshot(200, 1, &ok).is_some());

        // Gap.
        assert!(SegmentTable::from_snapshot(200, 1, &[sp(0, 100, 0), sp(150, 200, 150)]).is_none());
        // Overlap.
        assert!(SegmentTable::from_snapshot(200, 1, &[sp(0, 100, 0), sp(50, 200, 50)]).is_none());
        // Unsorted.
        assert!(SegmentTable::from_snapshot(200, 1, &[sp(100, 200, 100), sp(0, 100, 0)]).is_none());
        // pos past end, and pos before start.
        assert!(
            SegmentTable::from_snapshot(200, 1, &[sp(0, 100, 101), sp(100, 200, 100)]).is_none()
        );
        assert!(SegmentTable::from_snapshot(200, 1, &[sp(0, 100, 0), sp(100, 200, 99)]).is_none());
        // start >= end.
        assert!(SegmentTable::from_snapshot(200, 1, &[sp(0, 0, 0), sp(0, 200, 0)]).is_none());
        // Wrong total, and not starting at 0.
        assert!(SegmentTable::from_snapshot(300, 1, &ok).is_none());
        assert!(SegmentTable::from_snapshot(200, 1, &[sp(10, 200, 10)]).is_none());
        // Zero total: only the empty list is valid. Empty list for nonzero total is not.
        assert!(SegmentTable::from_snapshot(0, 1, &[sp(0, 0, 0)]).is_none());
        assert!(SegmentTable::from_snapshot(0, 1, &[]).is_some());
        assert!(SegmentTable::from_snapshot(5, 1, &[]).is_none());
    }

    /// Marks `n` bytes starting at `start` as written, asserting none was written before.
    fn write_range(written: &mut [bool], start: u64, n: u64) {
        let r = &mut written[start as usize..(start + n) as usize];
        assert!(r.iter().all(|b| !b), "byte written twice");
        r.fill(true);
    }

    /// Commits `n` reserved bytes of `id` and records them as written.
    fn commit_bytes(
        table: &SegmentTable,
        written: &mut [bool],
        written_count: &mut u64,
        id: SegmentId,
        n: u64,
    ) {
        if n == 0 {
            return;
        }
        let (pos, _) = table.range(id);
        write_range(written, pos, n);
        *written_count += n;
        table.commit(id, n);
    }

    const WORKERS: usize = 8;

    proptest! {
        #[test]
        fn random_worker_actions_write_every_byte_exactly_once(
            total in 1u64..2_000_000,
            connections in 1usize..=16,
            min_split in 1u64..100_000,
            // (worker, action kind 0..4, reserve amount)
            ops in prop::collection::vec((0..WORKERS, 0u8..4, 1u64..=70_000), 0..400),
        ) {
            let table = SegmentTable::new(total, connections, min_split);
            let mut written = vec![false; total as usize];
            let mut written_count = 0u64;
            // Segment each worker owns, and bytes it has reserved but not committed.
            let mut owned: [Option<SegmentId>; WORKERS] = [None; WORKERS];
            let mut pending = [0u64; WORKERS];

            for (w, kind, amount) in ops {
                match kind {
                    0 => {
                        if owned[w].is_none()
                            && let Some(id) = table.claim()
                        {
                            assert!(owned.iter().all(|o| *o != Some(id)), "segment claimed twice");
                            owned[w] = Some(id);
                        }
                    }
                    1 => {
                        if let Some(id) = owned[w] {
                            pending[w] += table.reserve(id, amount);
                        }
                    }
                    2 => {
                        if let Some(id) = owned[w] {
                            let n = std::mem::take(&mut pending[w]);
                            commit_bytes(&table, &mut written, &mut written_count, id, n);
                        }
                    }
                    _ => {
                        if let Some(id) = owned[w].take() {
                            // Uncommitted bytes are dropped by release.
                            pending[w] = 0;
                            table.release(id);
                        }
                    }
                }

                check_invariants(&table);
                assert_eq!(table.downloaded(), written_count);
                for (w, slot) in owned.iter().enumerate() {
                    if let Some(id) = *slot {
                        let (pos, end) = table.range(id);
                        assert!(pos + pending[w] <= end, "reserved past segment end");
                    }
                }
            }

            // Finish every worker, then drain the table with one worker.
            for w in 0..WORKERS {
                if let Some(id) = owned[w].take() {
                    let n = std::mem::take(&mut pending[w]);
                    commit_bytes(&table, &mut written, &mut written_count, id, n);
                    table.release(id);
                }
            }
            while let Some(id) = table.claim() {
                loop {
                    let got = table.reserve(id, 70_000);
                    if got == 0 {
                        break;
                    }
                    commit_bytes(&table, &mut written, &mut written_count, id, got);
                }
                table.release(id);
                check_invariants(&table);
            }

            assert!(table.is_complete());
            assert_eq!(table.downloaded(), total);
            assert_eq!(written_count, total);
            assert!(written.iter().all(|&b| b), "some byte was never written");
        }
    }
}
