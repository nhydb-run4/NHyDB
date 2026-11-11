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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kernels() {
        let a = [1, 3, 5, 7, 9];
        let b = [2, 3, 6, 7, 10];
        assert_eq!(intersect_sorted(&a, &b), vec![3, 7]);
        assert_eq!(union_sorted(&a, &b), vec![1, 2, 3, 5, 6, 7, 9, 10]);
        assert_eq!(difference_sorted(&a, &b), vec![1, 5, 9]);
        let big: Vec<i32> = (0..10000).collect();
        assert_eq!(intersect_sorted(&[5, 77, 20000], &big), vec![5, 77]);
        assert_eq!(sorted_unique(vec![7, 1, 7, 3]), vec![1, 3, 7]);
        let n = |x: u64| Nat::from(x);
        let r = [(n(1), n(4)), (n(8), n(9))];
        assert_eq!(ranges_minus_sorted(&r, &[n(0), n(2), n(4), n(9), n(12)]), vec![n(1), n(3), n(8)]);
        assert_eq!(ranges_minus_sorted(&r, &[]), vec![n(1), n(2), n(3), n(4), n(8), n(9)]);
        let big = Nat::from(u64::MAX);
        let rb = [(big.clone(), big.add_u64(2))];
        assert_eq!(ranges_minus_sorted(&rb, &[big.add_u64(1)]), vec![big.clone(), big.add_u64(2)]);
    }
}

#[cfg(test)]
mod stab_tests {
    use super::*;

    fn merge_ref(a: &[i64], b: &[i64]) -> Vec<i64> {
        let (mut i, mut j, mut out) = (0, 0, Vec::new());
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Equal => {
                    out.push(a[i]);
                    i += 1;
                    j += 1;
                }
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
            }
        }
        out
    }

    #[test]
    fn intersect_with_duplicates_f044() {
        let mut seed = 12345u64;
        let mut rnd = |m: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % m
        };
        for _ in 0..3000 {
            let mut a: Vec<i64> = (0..rnd(6)).map(|_| rnd(20) as i64).collect();
            let mut b: Vec<i64> = (0..rnd(200)).map(|_| rnd(20) as i64).collect();
            a.sort();
            b.sort();
            assert_eq!(intersect_sorted(&a, &b), merge_ref(&a, &b), "{a:?} {b:?}");
            assert_eq!(intersect_sorted(&b, &a), merge_ref(&a, &b), "{a:?} {b:?}");
        }
        let mut big: Vec<i64> = (0..100).chain([10, 10, 10]).collect();
        big.sort();
        assert_eq!(intersect_sorted(&[10, 10], &big), vec![10, 10]);
    }
}
