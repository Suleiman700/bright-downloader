//! Segment bookkeeping and dynamic splitting.
//!
//! A download is a set of byte ranges ("segments"). Each connection owns one
//! segment and streams it from an open-ended `Range: bytes=N-` request. When a
//! connection finishes its segment it steals the back half of the largest
//! remaining one: the victim's `end` shrinks and the victim simply stops
//! reading once it reaches the new end, so no request needs to be reissued.
//! This keeps every connection busy until the very last bytes.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Segment {
    pub id: usize,
    pub start: u64,
    /// Exclusive end; `None` while the total length is unknown.
    pub end: Option<u64>,
    /// Bytes written to disk, contiguous from `start`.
    pub written: u64,
    /// Bytes claimed by the owning worker (>= `written`). Splits never cut
    /// below this point, so in-flight writes are never invalidated.
    #[serde(skip)]
    pub reserved: u64,
    #[serde(skip)]
    pub owner: Option<usize>,
}

impl Segment {
    pub fn cursor(&self) -> u64 {
        self.start + self.reserved
    }

    pub fn write_offset(&self) -> u64 {
        self.start + self.written
    }

    pub fn remaining(&self) -> Option<u64> {
        self.end.map(|end| end.saturating_sub(self.cursor()))
    }

    pub fn is_done(&self) -> bool {
        self.remaining() == Some(0) && self.reserved == self.written
    }
}

#[derive(Debug, Clone)]
pub struct SegmentTable {
    segments: Vec<Segment>,
    next_id: usize,
    min_split: u64,
}

impl SegmentTable {
    /// Fresh table for a download of known size, pre-split into up to `parts`
    /// equal segments (never smaller than `min_split`).
    pub fn sized(total: u64, parts: usize, min_split: u64) -> Self {
        let max_parts = (total / min_split.max(1)).max(1) as usize;
        let parts = parts.clamp(1, max_parts) as u64;
        let base = total / parts;
        let segments = (0..parts)
            .map(|i| Segment {
                id: i as usize,
                start: i * base,
                end: Some(if i == parts - 1 { total } else { (i + 1) * base }),
                written: 0,
                reserved: 0,
                owner: None,
            })
            .collect();
        Self { segments, next_id: parts as usize, min_split }
    }

    /// Single open-ended segment for servers that do not report a length.
    pub fn open_ended() -> Self {
        let segments = vec![Segment { id: 0, start: 0, end: None, written: 0, reserved: 0, owner: None }];
        Self { segments, next_id: 1, min_split: u64::MAX }
    }

    /// Restore from persisted segments; in-flight reservations are discarded.
    pub fn restore(mut segments: Vec<Segment>, min_split: u64) -> Self {
        for s in &mut segments {
            s.reserved = s.written;
            s.owner = None;
        }
        let next_id = segments.iter().map(|s| s.id + 1).max().unwrap_or(0);
        Self { segments, next_id, min_split }
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn get(&self, id: usize) -> Option<&Segment> {
        self.segments.iter().find(|s| s.id == id)
    }

    fn get_mut(&mut self, id: usize) -> Option<&mut Segment> {
        self.segments.iter_mut().find(|s| s.id == id)
    }

    pub fn total_written(&self) -> u64 {
        self.segments.iter().map(|s| s.written).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.segments.iter().all(Segment::is_done)
    }

    /// Give `worker` a segment to download: an unowned unfinished one if any,
    /// otherwise the back half of the largest segment worth splitting.
    pub fn claim(&mut self, worker: usize) -> Option<usize> {
        if let Some(seg) = self
            .segments
            .iter_mut()
            .filter(|s| s.owner.is_none() && !s.is_done())
            .max_by_key(|s| s.remaining().unwrap_or(u64::MAX))
        {
            seg.owner = Some(worker);
            return Some(seg.id);
        }

        let min_split = self.min_split;
        let victim = self
            .segments
            .iter_mut()
            .filter(|s| s.owner.is_some())
            .filter_map(|s| s.remaining().map(|r| (r, s)))
            .filter(|(r, _)| *r >= min_split.saturating_mul(2))
            .max_by_key(|(r, _)| *r)
            .map(|(_, s)| s)?;

        let remaining = victim.remaining()?;
        let mid = victim.cursor() + remaining / 2;
        let old_end = victim.end.replace(mid);

        let id = self.next_id;
        self.next_id += 1;
        self.segments.push(Segment { id, start: mid, end: old_end, written: 0, reserved: 0, owner: Some(worker) });
        self.segments.sort_by_key(|s| s.start);
        Some(id)
    }

    /// Claim up to `want` bytes of segment `id` for writing. Returns the write
    /// offset and how many bytes may be written (0 means the segment is done,
    /// possibly because another worker took over its tail).
    pub fn reserve(&mut self, id: usize, want: u64) -> (u64, u64) {
        let Some(seg) = self.get_mut(id) else { return (0, 0) };
        let allowed = seg.remaining().map_or(want, |r| r.min(want));
        let offset = seg.cursor();
        seg.reserved += allowed;
        (offset, allowed)
    }

    /// Record that `n` reserved bytes of segment `id` reached the disk.
    pub fn commit(&mut self, id: usize, n: u64) {
        if let Some(seg) = self.get_mut(id) {
            seg.written = (seg.written + n).min(seg.reserved);
        }
    }

    /// Fix the length of an open-ended segment once its stream ends.
    pub fn close_open_ended(&mut self, id: usize) {
        if let Some(seg) = self.get_mut(id) {
            if seg.end.is_none() {
                seg.end = Some(seg.cursor());
            }
        }
    }

    /// Hand a segment back (worker failed or stopped). Unwritten reservations
    /// are dropped so the next owner resumes at the last durable byte.
    pub fn release(&mut self, id: usize) {
        if let Some(seg) = self.get_mut(id) {
            seg.reserved = seg.written;
            seg.owner = None;
        }
    }

    /// Throw away a segment's progress (servers without range support can
    /// only be re-read from the beginning).
    pub fn reset(&mut self, id: usize) {
        if let Some(seg) = self.get_mut(id) {
            seg.written = 0;
            seg.reserved = 0;
        }
    }

    pub fn active_owners(&self) -> usize {
        self.segments.iter().filter(|s| s.owner.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 1024;

    #[test]
    fn sized_splits_evenly_and_covers_whole_file() {
        let t = SegmentTable::sized(1000 * K, 4, 64 * K);
        let segs = t.segments();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[3].end, Some(1000 * K));
        for pair in segs.windows(2) {
            assert_eq!(pair[0].end, Some(pair[1].start));
        }
    }

    #[test]
    fn small_files_get_fewer_parts() {
        let t = SegmentTable::sized(100 * K, 8, 64 * K);
        assert_eq!(t.segments().len(), 1);
    }

    #[test]
    fn claim_prefers_unowned_then_steals_largest_half() {
        let mut t = SegmentTable::sized(1000 * K, 1, 64 * K);
        let a = t.claim(0).unwrap();
        // Worker 0 has written 200K of its 1000K segment.
        let (_, n) = t.reserve(a, 200 * K);
        t.commit(a, n);

        let b = t.claim(1).unwrap();
        let sa = t.get(a).unwrap().clone();
        let sb = t.get(b).unwrap().clone();
        assert_eq!(sa.end, Some(600 * K), "victim keeps the front half of what is left");
        assert_eq!(sb.start, 600 * K);
        assert_eq!(sb.end, Some(1000 * K));
    }

    #[test]
    fn no_split_below_twice_min_split() {
        let mut t = SegmentTable::sized(100 * K, 1, 64 * K);
        t.claim(0).unwrap();
        assert!(t.claim(1).is_none());
    }

    #[test]
    fn reserve_is_capped_by_shrunk_end() {
        let mut t = SegmentTable::sized(1000 * K, 1, 64 * K);
        let a = t.claim(0).unwrap();
        t.claim(1).unwrap(); // a now ends at 500K
        let (off, n) = t.reserve(a, 800 * K);
        assert_eq!((off, n), (0, 500 * K));
        let (_, n) = t.reserve(a, 1);
        assert_eq!(n, 0);
    }

    #[test]
    fn release_drops_unwritten_reservation() {
        let mut t = SegmentTable::sized(1000 * K, 1, 64 * K);
        let a = t.claim(0).unwrap();
        let (_, n) = t.reserve(a, 300 * K);
        t.commit(a, n - 100 * K);
        t.release(a);
        let s = t.get(a).unwrap();
        assert_eq!(s.reserved, 200 * K);
        assert_eq!(s.owner, None);
        // The next claimant resumes the same segment at the durable offset.
        assert_eq!(t.claim(5), Some(a));
        assert_eq!(t.get(a).unwrap().cursor(), 200 * K);
    }

    #[test]
    fn complete_when_all_segments_written() {
        let mut t = SegmentTable::sized(200 * K, 2, 64 * K);
        while let Some(id) = t.claim(0) {
            let (_, n) = t.reserve(id, u64::MAX);
            t.commit(id, n);
        }
        assert!(t.is_complete());
        assert_eq!(t.total_written(), 200 * K);
    }

    #[test]
    fn open_ended_segment_closes_at_cursor() {
        let mut t = SegmentTable::open_ended();
        let a = t.claim(0).unwrap();
        let (_, n) = t.reserve(a, 12345);
        t.commit(a, n);
        assert!(!t.is_complete());
        t.close_open_ended(a);
        assert!(t.is_complete());
        assert_eq!(t.get(a).unwrap().end, Some(12345));
    }

    #[test]
    fn restore_resets_runtime_fields() {
        let mut t = SegmentTable::sized(1000 * K, 2, 64 * K);
        let a = t.claim(0).unwrap();
        let (_, n) = t.reserve(a, 100 * K);
        t.commit(a, n / 2);
        let restored = SegmentTable::restore(t.segments().to_vec(), 64 * K);
        let s = restored.get(a).unwrap();
        assert_eq!(s.reserved, s.written);
        assert_eq!(s.owner, None);
    }
}
