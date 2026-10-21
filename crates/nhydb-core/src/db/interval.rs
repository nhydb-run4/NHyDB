//! Interval index over `[u_start, u_end]` ranges of segments.
//!
//! Entries are sorted by `(start, id)`; in addition the
//! prefix maximum of the end values is kept. A point query starts at the
//! last entry with `start <= u` and walks backwards until the
//! prefix maximum is smaller than `u`. For non-overlapping segments this is
//! O(log n) instead of a linear scan over all segments with overlays.

use crate::nat::Nat;

#[derive(Default, Clone, Debug)]
pub struct IntervalIndex {
    starts: Vec<Nat>,
    ends: Vec<Nat>,
    ids: Vec<usize>,
    max_end: Vec<Nat>,
}

impl IntervalIndex {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn insert(&mut self, start: Nat, end: Nat, id: usize) {
        let pos = self.starts.partition_point(|s| s <= &start);
        // equal starts: ascending id (insertion order = block order)
        let pos = {
            let mut p = pos;
            while p > 0 && self.starts[p - 1] == start && self.ids[p - 1] > id {
                p -= 1;
            }
            p
        };
        self.starts.insert(pos, start);
        self.ends.insert(pos, end);
        self.ids.insert(pos, id);
        self.max_end.insert(pos, Nat::ZERO);
        for i in pos..self.ids.len() {
            let prev = if i == 0 { None } else { Some(&self.max_end[i - 1]) };
            let m = match prev {
                Some(p) if p > &self.ends[i] => p.clone(),
                _ => self.ends[i].clone(),
            };
            if i > pos && self.max_end[i] == m {
                break;
            }
            self.max_end[i] = m;
        }
    }

    /// All ids whose interval contains `u` (unsorted).
    pub fn stab(&self, u: &Nat, out: &mut Vec<usize>) {
        let mut j = self.starts.partition_point(|s| s <= u);
        while j > 0 {
            j -= 1;
            if &self.max_end[j] < u {
                break;
            }
            if &self.ends[j] >= u {
                out.push(self.ids[j]);
            }
        }
    }

    /// All ids whose interval intersects `[lo, hi]`.
    pub fn overlapping(&self, lo: &Nat, hi: &Nat, out: &mut Vec<usize>) {
        let mut j = self.starts.partition_point(|s| s <= hi);
        while j > 0 {
            j -= 1;
            if &self.max_end[j] < lo {
                break;
            }
            if &self.ends[j] >= lo {
                out.push(self.ids[j]);
            }
        }
    }

    pub fn any_overlap(&self, lo: &Nat, hi: &Nat) -> bool {
        let mut j = self.starts.partition_point(|s| s <= hi);
        while j > 0 {
            j -= 1;
            if &self.max_end[j] < lo {
                return false;
            }
            if &self.ends[j] >= lo {
                return true;
            }
        }
        false
    }

    /// Smallest `x >= from` covered by some interval (P004): O(log n) via the
    /// prefix maximum of the end values instead of a scan over all segments.
    pub fn first_covered_from(&self, from: &Nat) -> Option<Nat> {
        let j = self.starts.partition_point(|s| s <= from);
        if j > 0 && &self.max_end[j - 1] >= from {
            return Some(from.clone());
        }
        self.starts.get(j).cloned()
    }

    /// ids in `(start, id)` order.
    pub fn ids_by_start(&self) -> &[usize] {
        &self.ids
    }

    /// id of the first interval (in start order) that starts after `u`.
    pub fn first_start_after(&self, u: &Nat) -> Option<usize> {
        self.ids.get(self.starts.partition_point(|s| s <= u)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stab_and_overlap() {
        let mut iv = IntervalIndex::default();
        iv.insert(Nat::S(1), Nat::S(10), 0);
        iv.insert(Nat::S(11), Nat::S(20), 1);
        iv.insert(Nat::S(5), Nat::S(6), 2);
        iv.insert(Nat::S(100), Nat::S(100), 3);
        let mut out = vec![];
        iv.stab(&Nat::S(5), &mut out);
        out.sort();
        assert_eq!(out, vec![0, 2]);
        out.clear();
        iv.stab(&Nat::S(15), &mut out);
        assert_eq!(out, vec![1]);
        out.clear();
        iv.stab(&Nat::S(50), &mut out);
        assert!(out.is_empty());
        assert!(iv.any_overlap(&Nat::S(20), &Nat::S(30)));
        assert!(!iv.any_overlap(&Nat::S(21), &Nat::S(99)));
        assert_eq!(iv.ids_by_start(), &[0, 2, 1, 3]);
    }

    #[test]
    fn randomized_against_bruteforce() {
        let mut seed = 12345u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % 1000
        };
        let mut iv = IntervalIndex::default();
        let mut all = vec![];
        for id in 0..300 {
            let a = rnd();
            let b = a + rnd() % 30;
            iv.insert(Nat::S(a), Nat::S(b), id);
            all.push((a, b));
        }
        for u in 0..1100 {
            let mut got = vec![];
            iv.stab(&Nat::S(u), &mut got);
            got.sort();
            let exp: Vec<usize> = all.iter().enumerate().filter(|(_, (a, b))| *a <= u && u <= *b).map(|(i, _)| i).collect();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn first_covered_from_against_bruteforce() {
        let mut seed = 777u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % 2000
        };
        let mut iv = IntervalIndex::default();
        let mut all = vec![];
        assert_eq!(iv.first_covered_from(&Nat::S(1)), None);
        for id in 0..200 {
            let a = rnd() + 1;
            let b = a + rnd() % 15;
            iv.insert(Nat::S(a), Nat::S(b), id);
            all.push((a, b));
            if id % 37 == 0 {
                for from in 0..2100 {
                    let exp = all.iter().filter(|(_, b)| *b >= from).map(|(a, _)| (*a).max(from)).min();
                    assert_eq!(iv.first_covered_from(&Nat::S(from)), exp.map(Nat::S), "from={from}");
                }
            }
        }
    }
}
