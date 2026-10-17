//! Sortierte Mengenkerne (`sorted_unique_ints`, `intersect/union/
//! difference_sorted_ints`) über aufsteigend sortierten Folgen.

/// Schnitt zweier sortierter Folgen (galoppierend bei stark ungleichen Längen).
pub fn intersect_sorted<T: Ord + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(small.len());
    if small.len() * 16 < big.len() {
        let mut lo = 0usize;
        for x in small {
            match big[lo..].binary_search(x) {
                Ok(i) => {
                    out.push(x.clone());
                    lo += i + 1;
                }
                Err(i) => lo += i,
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

/// Vereinigung ohne Duplikate.
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

/// Differenz A \ B.
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

/// Sortiert und entfernt Duplikate.
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
    }
}
