//! Segment index of one `(realm, lane)`: persisted directory runs plus an
//! in-memory tail (format 0.17, block directory).
//!
//! The tail holds the segments registered since the last directory flush in
//! block order, indexed by an [`IntervalIndex`]. Older segments live in
//! immutable runs on disk: each run part is a list of directory pages sorted
//! by `(lo, block_id)`; only the page summaries (first `lo`, largest `hi`)
//! are held in memory, the pages themselves are loaded on demand through a
//! [`PageSource`] (CRC-checked, cached with a budget). All queries combine
//! the runs and the tail and have the semantics of the former interval
//! index over all segments.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use crate::error::Result;
use crate::nat::Nat;

use super::interval::IntervalIndex;

/// A directory entry: an interval `[lo, hi]` of u owned by one block.
pub(crate) trait SegEntry: Clone + Send + Sync + 'static {
    fn lo(&self) -> &Nat;
    fn hi(&self) -> &Nat;
    fn bid(&self) -> u64;
}

/// Decoded directory page: entries sorted by `(lo, bid)` and the prefix
/// maximum of `hi`.
#[derive(Debug)]
pub(crate) struct Page<T> {
    pub entries: Vec<T>,
    pub pmax: Vec<Nat>,
}

impl<T: SegEntry> Page<T> {
    pub(crate) fn new(entries: Vec<T>) -> Page<T> {
        let mut pmax: Vec<Nat> = Vec::with_capacity(entries.len());
        for e in &entries {
            let m = match pmax.last() {
                Some(p) if p > e.hi() => p.clone(),
                _ => e.hi().clone(),
            };
            pmax.push(m);
        }
        Page { entries, pmax }
    }
}

/// Summary of one directory page held in memory.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PageRef {
    /// Offset of the directory block.
    pub off: u64,
    /// Number of entries.
    pub n: u32,
    /// `lo` of the first entry.
    pub first: Nat,
    /// Largest `hi` of the page.
    pub max_hi: Nat,
    /// Largest block id of the page (newest-wins lookups skip pages that
    /// cannot hold a newer entry).
    pub max_bid: u64,
}

/// The pages of one `(kind, realm, lane)` inside one directory run.
#[derive(Clone, Debug)]
pub(crate) struct RunPart {
    /// offset of the run block this part belongs to
    pub run: u64,
    pub pages: Vec<PageRef>,
    /// prefix maximum of `max_hi` over the pages
    pub pmax: Vec<Nat>,
    /// number of entries
    pub n: u64,
}

impl RunPart {
    pub(crate) fn new(run: u64, pages: Vec<PageRef>) -> RunPart {
        let mut pmax: Vec<Nat> = Vec::with_capacity(pages.len());
        for p in &pages {
            let m = match pmax.last() {
                Some(x) if *x > p.max_hi => x.clone(),
                _ => p.max_hi.clone(),
            };
            pmax.push(m);
        }
        let n = pages.iter().map(|p| p.n as u64).sum();
        RunPart { run, pages, pmax, n }
    }
}

/// Loads directory pages (implemented by the database handle).
pub(crate) trait PageSource<T> {
    fn load_page(&self, p: &PageRef) -> Result<Arc<Page<T>>>;
}

/// Segment index of one key.
#[derive(Clone, Debug)]
pub(crate) struct SegIndex<T> {
    /// persisted run parts (order irrelevant for queries)
    pub runs: Vec<Arc<RunPart>>,
    /// segments since the last flush, block order
    pub tail: Vec<T>,
    tail_iv: IntervalIndex,
    /// number of entries in `runs`
    pub flushed: u64,
}

impl<T> Default for SegIndex<T> {
    fn default() -> Self {
        SegIndex { runs: Vec::new(), tail: Vec::new(), tail_iv: IntervalIndex::default(), flushed: 0 }
    }
}

impl<T: SegEntry> SegIndex<T> {
    /// Total number of entries (runs + tail); grows monotonically within a
    /// session (registration count).
    pub(crate) fn len(&self) -> u64 {
        self.flushed + self.tail.len() as u64
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn push(&mut self, e: T) {
        self.tail_iv.insert(e.lo().clone(), e.hi().clone(), self.tail.len());
        self.tail.push(e);
    }

    pub(crate) fn add_run(&mut self, part: Arc<RunPart>) {
        self.flushed += part.n;
        self.runs.push(part);
    }

    /// Replaces the tail by nothing (after a flush wrote it into a run).
    pub(crate) fn clear_tail(&mut self) {
        self.tail.clear();
        self.tail_iv = IntervalIndex::default();
    }

    /// Replaces all runs (after a merge).
    pub(crate) fn set_runs(&mut self, runs: Vec<Arc<RunPart>>) {
        self.flushed = runs.iter().map(|r| r.n).sum();
        self.runs = runs;
    }

    /// Tail entries registered after `seen` entries, if they are all still
    /// in the tail.
    pub(crate) fn tail_since(&self, seen: u64) -> Option<&[T]> {
        if seen < self.flushed {
            return None;
        }
        let i = (seen - self.flushed) as usize;
        self.tail.get(i..)
    }

    /// Visits every entry of the runs whose page may intersect `[lo, hi]`
    /// and that satisfies `lo_e <= hi && hi_e >= lo`.
    fn runs_overlapping(&self, src: &dyn PageSource<T>, lo: &Nat, hi: &Nat, f: &mut dyn FnMut(&T) -> bool) -> Result<bool> {
        for part in &self.runs {
            let mut j = part.pages.partition_point(|p| &p.first <= hi);
            while j > 0 {
                j -= 1;
                if &part.pmax[j] < lo {
                    break;
                }
                if &part.pages[j].max_hi < lo {
                    continue;
                }
                let page = src.load_page(&part.pages[j])?;
                let mut k = page.entries.partition_point(|e| e.lo() <= hi);
                while k > 0 {
                    k -= 1;
                    if &page.pmax[k] < lo {
                        break;
                    }
                    let e = &page.entries[k];
                    if e.hi() >= lo && f(e) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Calls `f` for every entry containing `u` whose block id is larger
    /// than `newer_than` (unordered; pages without such an entry are not
    /// loaded).
    pub(crate) fn visit_containing(&self, src: &dyn PageSource<T>, u: &Nat, newer_than: u64, f: &mut dyn FnMut(&T) -> Result<()>) -> Result<()> {
        let mut err = None;
        self.tail_iv.stab_each(u, |i| {
            if err.is_none() && self.tail[i].bid() > newer_than {
                if let Err(e) = f(&self.tail[i]) {
                    err = Some(e);
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        for part in &self.runs {
            let mut j = part.pages.partition_point(|p| &p.first <= u);
            while j > 0 {
                j -= 1;
                if &part.pmax[j] < u {
                    break;
                }
                let pr = &part.pages[j];
                if &pr.max_hi < u || pr.max_bid <= newer_than {
                    continue;
                }
                let page = src.load_page(pr)?;
                let mut k = page.entries.partition_point(|e| e.lo() <= u);
                while k > 0 {
                    k -= 1;
                    if &page.pmax[k] < u {
                        break;
                    }
                    let e = &page.entries[k];
                    if e.hi() >= u && e.bid() > newer_than {
                        f(e)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// All entries whose interval contains `u` (unordered).
    pub(crate) fn stab(&self, src: &dyn PageSource<T>, u: &Nat, out: &mut Vec<T>) -> Result<()> {
        let mut ids = Vec::new();
        self.tail_iv.stab(u, &mut ids);
        out.extend(ids.into_iter().map(|i| self.tail[i].clone()));
        if !self.runs.is_empty() {
            self.runs_overlapping(src, u, u, &mut |e| {
                out.push(e.clone());
                false
            })?;
        }
        Ok(())
    }

    /// The entry with the largest block id containing `u` (newest wins),
    /// cloned once.
    pub(crate) fn stab_newest(&self, src: &dyn PageSource<T>, u: &Nat) -> Result<Option<T>> {
        let mut best: Option<usize> = None;
        self.tail_iv.stab_each(u, |i| {
            if best.is_none_or(|b| self.tail[i].bid() > self.tail[b].bid()) {
                best = Some(i);
            }
        });
        let mut out = best.map(|i| self.tail[i].clone());
        for part in &self.runs {
            let mut j = part.pages.partition_point(|p| &p.first <= u);
            while j > 0 {
                j -= 1;
                if &part.pmax[j] < u {
                    break;
                }
                let pr = &part.pages[j];
                if &pr.max_hi < u || out.as_ref().is_some_and(|b| pr.max_bid <= b.bid()) {
                    continue;
                }
                let page = src.load_page(pr)?;
                let mut k = page.entries.partition_point(|e| e.lo() <= u);
                while k > 0 {
                    k -= 1;
                    if &page.pmax[k] < u {
                        break;
                    }
                    let e = &page.entries[k];
                    if e.hi() >= u && out.as_ref().is_none_or(|b| e.bid() > b.bid()) {
                        out = Some(e.clone());
                    }
                }
            }
        }
        Ok(out)
    }

    /// Whether any entry contains `u`.
    pub(crate) fn covers(&self, src: &dyn PageSource<T>, u: &Nat) -> Result<bool> {
        self.any_overlap(src, u, u)
    }

    /// All entries intersecting `[lo, hi]` (unordered).
    pub(crate) fn overlapping(&self, src: &dyn PageSource<T>, lo: &Nat, hi: &Nat, out: &mut Vec<T>) -> Result<()> {
        let mut ids = Vec::new();
        self.tail_iv.overlapping(lo, hi, &mut ids);
        out.extend(ids.into_iter().map(|i| self.tail[i].clone()));
        if !self.runs.is_empty() {
            self.runs_overlapping(src, lo, hi, &mut |e| {
                out.push(e.clone());
                false
            })?;
        }
        Ok(())
    }

    pub(crate) fn any_overlap(&self, src: &dyn PageSource<T>, lo: &Nat, hi: &Nat) -> Result<bool> {
        if self.tail_iv.any_overlap(lo, hi) {
            return Ok(true);
        }
        if self.runs.is_empty() {
            return Ok(false);
        }
        self.runs_overlapping(src, lo, hi, &mut |_| true)
    }

    /// Entry with the smallest `(lo, bid)` among those with `lo > u`.
    pub(crate) fn first_start_after(&self, src: &dyn PageSource<T>, u: &Nat) -> Result<Option<T>> {
        let mut best: Option<T> = self.tail_iv.first_start_after(u).map(|i| self.tail[i].clone());
        let better = |cand: &T, best: &Option<T>| match best {
            None => true,
            Some(b) => (cand.lo(), cand.bid()) < (b.lo(), b.bid()),
        };
        for part in &self.runs {
            // the first page whose first lo > u, and the page before it
            // (it may contain entries with lo > u after its first one)
            let j = part.pages.partition_point(|p| &p.first <= u);
            if j > 0 {
                let page = src.load_page(&part.pages[j - 1])?;
                let k = page.entries.partition_point(|e| e.lo() <= u);
                if let Some(e) = page.entries.get(k) {
                    if better(e, &best) {
                        best = Some(e.clone());
                    }
                    continue;
                }
            }
            if let Some(p) = part.pages.get(j) {
                let page = src.load_page(p)?;
                if let Some(e) = page.entries.first() {
                    if better(e, &best) {
                        best = Some(e.clone());
                    }
                }
            }
        }
        Ok(best)
    }

    /// Smallest `x >= from` covered by some entry.
    pub(crate) fn first_covered_from(&self, src: &dyn PageSource<T>, from: &Nat) -> Result<Option<Nat>> {
        if self.tail_iv.first_covered_from(from).as_ref() == Some(from) {
            return Ok(Some(from.clone()));
        }
        if self.covers(src, from)? {
            return Ok(Some(from.clone()));
        }
        Ok(self.first_start_after(src, from)?.map(|e| e.lo().clone()))
    }

    /// Owned iterator over all entries in `(lo, bid)` order (snapshot of the
    /// current runs and tail).
    pub(crate) fn iter_by_start(&self) -> ByStart<T> {
        let mut tail: Vec<T> = self.tail_iv.ids_by_start().iter().map(|&i| self.tail[i].clone()).collect();
        // equal starts are ordered by insertion = block order already
        tail.reverse();
        ByStart {
            runs: self.runs.iter().map(|p| RunCur { part: p.clone(), page_i: 0, page: None, ent_i: 0 }).collect(),
            tail,
            heap: BinaryHeap::new(),
            primed: false,
        }
    }

    /// All entries in `(lo, bid)` order.
    pub(crate) fn collect_by_start(&self, src: &dyn PageSource<T>) -> Result<Vec<T>> {
        let mut it = self.iter_by_start();
        let mut out = Vec::with_capacity(self.len().min(1 << 20) as usize);
        while let Some(e) = it.next(src)? {
            out.push(e);
        }
        Ok(out)
    }

    /// All entries in block order.
    pub(crate) fn collect_block_order(&self, src: &dyn PageSource<T>) -> Result<Vec<T>> {
        if self.runs.is_empty() {
            return Ok(self.tail.clone());
        }
        let mut out = self.collect_by_start(src)?;
        out.sort_by_key(|e| e.bid());
        Ok(out)
    }
}

struct RunCur<T> {
    part: Arc<RunPart>,
    page_i: usize,
    page: Option<Arc<Page<T>>>,
    ent_i: usize,
}

impl<T: SegEntry> RunCur<T> {
    fn peek(&mut self, src: &dyn PageSource<T>) -> Result<Option<T>> {
        loop {
            if self.page.is_none() {
                let Some(p) = self.part.pages.get(self.page_i) else { return Ok(None) };
                self.page = Some(src.load_page(p)?);
                self.ent_i = 0;
            }
            let page = self.page.as_ref().unwrap();
            if let Some(e) = page.entries.get(self.ent_i) {
                return Ok(Some(e.clone()));
            }
            self.page = None;
            self.page_i += 1;
        }
    }
}

/// k-way merge over the runs and the tail of a [`SegIndex`] in `(lo, bid)`
/// order. Holds only `Arc`s of immutable run parts, one page per run and a
/// copy of the tail; pages are loaded through the source passed to `next`.
pub(crate) struct ByStart<T> {
    runs: Vec<RunCur<T>>,
    /// reversed `(lo, bid)` order (pop from the back)
    tail: Vec<T>,
    heap: BinaryHeap<Reverse<(Nat, u64, usize)>>,
    primed: bool,
}

impl<T: SegEntry> ByStart<T> {
    pub(crate) fn next(&mut self, src: &dyn PageSource<T>) -> Result<Option<T>> {
        if !self.primed {
            for i in 0..self.runs.len() {
                if let Some(e) = self.runs[i].peek(src)? {
                    self.heap.push(Reverse((e.lo().clone(), e.bid(), i)));
                }
            }
            self.primed = true;
        }
        let tail_head = self.tail.last().map(|e| (e.lo().clone(), e.bid()));
        let run_head = self.heap.peek().map(|Reverse((lo, bid, _))| (lo.clone(), *bid));
        let take_tail = match (&tail_head, &run_head) {
            (None, None) => return Ok(None),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(t), Some(r)) => t < r,
        };
        if take_tail {
            return Ok(self.tail.pop());
        }
        let Reverse((_, _, i)) = self.heap.pop().expect("peeked");
        let cur = &mut self.runs[i];
        let e = cur.peek(src)?.expect("run head present");
        cur.ent_i += 1;
        if let Some(nx) = cur.peek(src)? {
            self.heap.push(Reverse((nx.lo().clone(), nx.bid(), i)));
        }
        Ok(Some(e))
    }
}

#[cfg(all(test, nhydb_internal_tests))]
#[path = "segindex_tests.rs"]
mod tests;
