//! Sorted set kernels (`sorted_unique_ints`, `intersect/union/
//! difference_sorted_ints`) over ascending sorted sequences.

use crate::nat::Nat;

/// Intersection of two sorted sequences (galloping when lengths differ greatly).
pub fn intersect_sorted<T: Ord + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(small.len());
    if small.len() * 16 < big.len() {
        let mut lo = 0usize;
        for x in small {
            // first element >= x (F044: with duplicates `binary_search` may hit
            // a later equal element and skip matches; multiset semantics as in
            // the merge path / reference)
            let i = big[lo..].partition_point(|y| y < x);
            if big.get(lo + i) == Some(x) {
                out.push(x.clone());
                lo += i + 1;
            } else {
                lo += i;
            }
            if lo >= big.len() {
                break;
            }
        }
        return out;
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                out.push(a[i].clone());
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    out
}

/// Union without duplicates.
pub fn union_sorted<T: Ord + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    let push = |out: &mut Vec<T>, x: &T| {
        if out.last() != Some(x) {
            out.push(x.clone());
        }
    };
    while i < a.len() || j < b.len() {
        if j >= b.len() || (i < a.len() && a[i] <= b[j]) {
            push(&mut out, &a[i]);
            i += 1;
        } else {
            push(&mut out, &b[j]);
            j += 1;
        }
    }
    out
}

/// Difference A \ B.
pub fn difference_sorted<T: Ord + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for x in a {
        while j < b.len() && &b[j] < x {
            j += 1;
        }
        if j >= b.len() || &b[j] != x {
            out.push(x.clone());
        }
    }
    out
}

/// All u of the sorted, disjoint inclusive ranges that are not in the
/// sorted list `b` (NOT over the metadata universe without expanding it
/// first).
pub fn ranges_minus_sorted(ranges: &[(Nat, Nat)], b: &[Nat]) -> Vec<Nat> {
    let mut out = Vec::new();
    let mut j = 0;
    for (lo, hi) in ranges {
        let mut u = lo.clone();
        while &u <= hi {
            while j < b.len() && b[j] < u {
                j += 1;
            }
            if j >= b.len() || b[j] != u {
                out.push(u.clone());
            }
            u = u.add_u64(1);
        }
    }
    out
}

/// Sorts and removes duplicates.
pub fn sorted_unique<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v.dedup();
    v
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "setops_tests.rs"]
mod tests;

#[cfg(all(test, nhydb_internal_tests))]
#[path = "setops_stab_tests.rs"]
mod stab_tests;
