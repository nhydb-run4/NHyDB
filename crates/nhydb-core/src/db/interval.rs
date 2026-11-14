//! Interval index over `[start, end]` ranges (segments of a realm/lane,
//! v ranges of sparse records).
//!
//! Entries are kept sorted by `(start, id)` in blocks of at most
//! [`BLOCK_MAX`] entries; every block holds the prefix maximum of its end
//! values and the index holds the prefix maximum over the blocks. A point
//! query starts at the last entry with `start <= u` and walks backwards
//! until the prefix maximum is smaller than `u` (O(log n) for
//! non-overlapping ranges). An insert touches one block (O(BLOCK_MAX +
//! log n)), so building the index costs O(n log n) for every insertion
//! order – a single sorted vector needed O(n) per out-of-order insert,
//! O(n²) in total (G1).

use crate::nat::Nat;

/// Entries per block before it is split into two halves.
const BLOCK_MAX: usize = 512;

#[derive(Default, Clone, Debug)]
struct Block {
    starts: Vec<Nat>,
    ends: Vec<Nat>,
    ids: Vec<usize>,
    /// prefix maximum of `ends` inside the block
    pmax: Vec<Nat>,
}

impl Block {
    fn len(&self) -> usize {
        self.ids.len()
    }

    fn max(&self) -> &Nat {
        self.pmax.last().expect("blocks are never empty")
    }

    /// First position whose `(start, id)` is greater than the key.
    fn upper(&self, start: &Nat, id: usize) -> usize {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if (&self.starts[mid], self.ids[mid]) <= (start, id) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Number of entries with `start <= u`.
    fn count_le(&self, u: &Nat) -> usize {
        self.starts.partition_point(|s| s <= u)
    }

    fn fix_pmax(&mut self, from: usize) {
        for i in from..self.len() {
            let m = match i.checked_sub(1).map(|j| &self.pmax[j]) {
                Some(p) if p > &self.ends[i] => p.clone(),
                _ => self.ends[i].clone(),
            };
            if i > from && self.pmax[i] == m {
                break;
            }
            self.pmax[i] = m;
        }
    }

    /// Recomputes the prefix maximum from `from` on after an entry before
    /// it was removed (stops once a stored value is confirmed).
    fn recompute_pmax(&mut self, from: usize) {
        self.fix_pmax(from);
    }

    fn split_off(&mut self, at: usize) -> Block {
        let mut b = Block {
            starts: self.starts.split_off(at),
            ends: self.ends.split_off(at),
            ids: self.ids.split_off(at),
            pmax: Vec::new(),
        };
        self.pmax.truncate(at);
        // full prefix maximum (no early stop: nothing is valid yet)
        let mut m: Option<Nat> = None;
        for e in &b.ends {
            let x = match m {
                Some(p) if p > *e => p,
                _ => e.clone(),
            };
            b.pmax.push(x.clone());
            m = Some(x);
        }
        b
    }
}

#[derive(Default, Clone, Debug)]
pub struct IntervalIndex {
    blocks: Vec<Block>,
    /// prefix maximum of the block maxima
    bmax: Vec<Nat>,
    n: usize,
}

impl IntervalIndex {
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn insert(&mut self, start: Nat, end: Nat, id: usize) {
        self.n += 1;
        if self.blocks.is_empty() {
            self.blocks.push(Block { starts: vec![start], ends: vec![end.clone()], ids: vec![id], pmax: vec![end.clone()] });
            self.bmax.push(end);
            return;
        }
        // fast path: appended behind the last entry (ascending inserts)
        let last = self.blocks.len() - 1;
        let lb = &self.blocks[last];
        let bi = if (&lb.starts[lb.len() - 1], lb.ids[lb.len() - 1]) <= (&start, id) {
            last
        } else {
            // last block whose first entry is not greater than the new one
            self.blocks.partition_point(|b| (&b.starts[0], b.ids[0]) <= (&start, id)).saturating_sub(1)
        };
        let b = &mut self.blocks[bi];
        let pos = b.upper(&start, id);
        b.starts.insert(pos, start);
        b.ends.insert(pos, end);
        b.ids.insert(pos, id);
        b.pmax.insert(pos, Nat::ZERO);
        b.fix_pmax(pos);
        if b.len() > BLOCK_MAX {
            let right = b.split_off(b.len() / 2);
            let m = right.max().clone();
            self.blocks.insert(bi + 1, right);
            self.bmax.insert(bi + 1, m);
        }
        self.fix_bmax(bi);
    }

    /// Removes the entry `(start, id)`; `false` if it is not present.
    pub fn remove(&mut self, start: &Nat, id: usize) -> bool {
        let bi = self.blocks.partition_point(|b| (&b.starts[0], b.ids[0]) <= (start, id));
        let Some(bi) = bi.checked_sub(1) else { return false };
        let b = &mut self.blocks[bi];
        let pos = b.upper(start, id);
        if pos == 0 || &b.starts[pos - 1] != start || b.ids[pos - 1] != id {
            return false;
        }
        let pos = pos - 1;
        b.starts.remove(pos);
        b.ends.remove(pos);
        b.ids.remove(pos);
        b.pmax.remove(pos);
        self.n -= 1;
        if b.len() == 0 {
            self.blocks.remove(bi);
            self.bmax.remove(bi);
            if bi < self.blocks.len() {
                self.recompute_bmax(bi);
            }
            return true;
        }
        if pos < b.len() {
            b.recompute_pmax(pos);
        }
        self.recompute_bmax(bi);
        true
    }

    /// Prefix maxima of the blocks from `from` on after a value may have
    /// decreased (stops once a stored value is confirmed).
    fn recompute_bmax(&mut self, from: usize) {
        for i in from..self.blocks.len() {
            let own = self.blocks[i].max();
            let m = match i.checked_sub(1).map(|j| &self.bmax[j]) {
                Some(p) if p > own => p.clone(),
                _ => own.clone(),
            };
            if i > from && self.bmax[i] == m {
                break;
            }
            self.bmax[i] = m;
        }
    }

    fn fix_bmax(&mut self, from: usize) {
        for i in from..self.blocks.len() {
            let own = self.blocks[i].max();
            let m = match i.checked_sub(1).map(|j| &self.bmax[j]) {
                Some(p) if p > own => p.clone(),
                _ => own.clone(),
            };
            if i > from + 1 && self.bmax[i] == m {
                break;
            }
            self.bmax[i] = m;
        }
    }

    /// Calls `f` for every id whose interval intersects `[lo, hi]`; stops
    /// when `f` returns `true` (result `true`).
    fn visit(&self, lo: &Nat, hi: &Nat, mut f: impl FnMut(usize) -> bool) -> bool {
        let j = self.blocks.partition_point(|b| &b.starts[0] <= hi);
        let mut bj = j;
        while bj > 0 {
            bj -= 1;
            if &self.bmax[bj] < lo {
                break;
            }
            let b = &self.blocks[bj];
            if b.max() < lo {
                continue;
            }
            let mut k = if bj + 1 == j { b.count_le(hi) } else { b.len() };
            while k > 0 {
                k -= 1;
                if &b.pmax[k] < lo {
                    break;
                }
                if &b.ends[k] >= lo && f(b.ids[k]) {
                    return true;
                }
            }
        }
        false
    }

    /// All ids whose interval contains `u` (unordered).
    pub fn stab(&self, u: &Nat, out: &mut Vec<usize>) {
        self.visit(u, u, |i| {
            out.push(i);
            false
        });
    }

    /// Calls `f` for every id whose interval contains `u` (no allocation).
    #[inline]
    pub fn stab_each(&self, u: &Nat, mut f: impl FnMut(usize)) {
        self.visit(u, u, |i| {
            f(i);
            false
        });
    }

    /// All ids whose interval intersects `[lo, hi]`.
    pub fn overlapping(&self, lo: &Nat, hi: &Nat, out: &mut Vec<usize>) {
        self.visit(lo, hi, |i| {
            out.push(i);
            false
        });
    }

    pub fn any_overlap(&self, lo: &Nat, hi: &Nat) -> bool {
        self.visit(lo, hi, |_| true)
    }

    /// Block index and position of the first entry with `start > u`
    /// (`None`: there is none).
    fn first_after(&self, u: &Nat) -> Option<(usize, usize)> {
        let j = self.blocks.partition_point(|b| &b.starts[0] <= u);
        if j > 0 {
            let k = self.blocks[j - 1].count_le(u);
            if k < self.blocks[j - 1].len() {
                return Some((j - 1, k));
            }
        }
        (j < self.blocks.len()).then_some((j, 0))
    }

    /// Smallest `x >= from` covered by some interval (P004): O(log n) via the
    /// prefix maxima of the end values instead of a scan over all segments.
    pub fn first_covered_from(&self, from: &Nat) -> Option<Nat> {
        let j = self.blocks.partition_point(|b| &b.starts[0] <= from);
        if j > 0 {
            if j >= 2 && &self.bmax[j - 2] >= from {
                return Some(from.clone());
            }
            let b = &self.blocks[j - 1];
            let k = b.count_le(from);
            if k > 0 && &b.pmax[k - 1] >= from {
                return Some(from.clone());
            }
        }
        self.first_after(from).map(|(b, k)| self.blocks[b].starts[k].clone())
    }

    /// ids in `(start, id)` order.
    pub fn ids_by_start(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.n);
        for b in &self.blocks {
            out.extend_from_slice(&b.ids);
        }
        out
    }

    /// id of the first interval (in start order) that starts after `u`.
    pub fn first_start_after(&self, u: &Nat) -> Option<usize> {
        self.first_after(u).map(|(b, k)| self.blocks[b].ids[k])
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "interval_tests.rs"]
mod tests;
