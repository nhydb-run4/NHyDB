//! RUN¹⁰ AP10.3: the decision logic of the limiter as a pure function
//! (plan §2.6), tested with tables of artificial states.
//!
//! | zone | decision |
//! |---|---|
//! | green | lift every `WAIT` (`RESUME`); end a cache relief |
//! | yellow | ① cache relief down to the yellow border (with feedback: when the last one did not lower the level, the relief ends – R9B-02); ② `SHRINK` to the elastic job with the largest growth since it last gave memory back; ③ jobs registered during the yellow/red phase `WAIT` |
//! | red | every job but one `WAIT`; the one that runs is the oldest job (lowest id) |
//! | always | liveness: if jobs are registered and none may run, the oldest one runs |
//! | yellow/red | a job on a `TEST_IT` probe (`Probe`, AP10.5) is left alone: it holds by itself when its probe is used up |
//!
//! Last resort (decision N5, `[limiter] cancel_over_limit`, default on): when
//! the level stays above the limit for [`CANCEL_AFTER_TICKS`] ticks
//! although every other job waits, and the one job that runs still grew in
//! that time, it gets `CANCEL` (`ResourceLimitError`, reason
//! `cancelled_by_limiter`); the next one runs. A further cancel needs real
//! growth: the window counts again only once the level is more than
//! [`CANCEL_REARM_MARGIN`] above the level of the last cancel (the memory of
//! a cancelled job comes back slowly, the Windows heap took ~20 s; without
//! this rule the next job was cancelled 100 ms later, and so on). Switched
//! off, the situation is only reported (`over_limit_single`).

use super::job::{JobKind, SlotState};
use super::Zone;

/// What the policy sees of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobView {
    /// registration order (smaller = older)
    pub id: u64,
    pub kind: JobKind,
    pub state: SlotState,
    /// growth reported since the job last acknowledged a `SHRINK`
    pub grown_since_shrink: u64,
    /// registered after the current yellow/red phase began
    pub new_in_phase: bool,
    /// growth reported since the level went above the limit (N5)
    pub grown_since_over: u64,
}

/// What the policy sees of the process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Situation {
    pub zone: Zone,
    pub level: u64,
    pub yellow_at: u64,
    pub limit: u64,
    /// bytes held by the cache pot now
    pub cache_used: u64,
    /// a relief was requested before in this phase and the level did not
    /// fall afterwards
    pub relief_ineffective: bool,
    /// N5: `[limiter] cancel_over_limit`
    pub cancel_over_limit: bool,
    /// ticks the level has been above the limit (since the last cancel)
    pub over_limit_ticks: u32,
}

/// N5: ticks above the limit before the single running job that keeps
/// growing is cancelled (≈ 100 ms at the default tick).
pub const CANCEL_AFTER_TICKS: u32 = 5;

/// N5: after a cancel, the next one needs the level this far above the
/// level of that cancel (the memory of a cancelled job comes back slowly,
/// e.g. the Windows heap; meanwhile the next job may grow into it).
pub const CANCEL_REARM_MARGIN: u64 = 64 << 20;

/// Decisions of one tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// new signal per job id (only changes)
    pub signals: Vec<(u64, SlotState)>,
    /// ask the cache pot to shrink to this many bytes (`None` = no relief;
    /// `Some(u64::MAX)` = end of a relief)
    pub cache_target: Option<u64>,
    /// a single running job keeps the level above the limit (N5: only
    /// reported, never cancelled by the limiter)
    pub over_limit_single: bool,
}

/// Growth an elastic job must have reported since its last `SHRINK`
/// before it is asked again (it has something to give back).
pub const SHRINK_MIN_GROWTH: u64 = 8 << 20;

pub fn policy(s: &Situation, jobs: &[JobView]) -> Decision {
    let mut d = Decision::default();
    let set = |d: &mut Decision, j: &JobView, to: SlotState| {
        if j.state != to && j.state != SlotState::Cancel {
            d.signals.push((j.id, to));
        }
    };
    let oldest = jobs.iter().filter(|j| j.state != SlotState::Cancel).map(|j| j.id).min();
    match s.zone {
        Zone::Green => {
            for j in jobs {
                if matches!(j.state, SlotState::Wait | SlotState::Shrink | SlotState::Probe) {
                    set(&mut d, j, SlotState::Go);
                }
            }
            d.cache_target = Some(u64::MAX);
        }
        Zone::Yellow => {
            if s.relief_ineffective {
                // R9B-02: the relief did not lower the level – end it, the
                // shrunk pot would only cost speed (block reloads)
                d.cache_target = Some(u64::MAX);
            } else if s.cache_used > 0 && s.level > s.yellow_at {
                let excess = (s.level - s.yellow_at).min(s.cache_used);
                d.cache_target = Some(s.cache_used - excess);
            }
            let shrink = jobs
                .iter()
                .filter(|j| j.kind == JobKind::Elastic && j.state == SlotState::Go && j.grown_since_shrink >= SHRINK_MIN_GROWTH)
                .max_by_key(|j| (j.grown_since_shrink, std::cmp::Reverse(j.id)));
            if let Some(j) = shrink {
                set(&mut d, j, SlotState::Shrink);
            }
            for j in jobs {
                if j.state == SlotState::Probe {
                    // a running TEST_IT probe ends by itself (AP10.5)
                } else if j.new_in_phase && Some(j.id) != oldest {
                    set(&mut d, j, SlotState::Wait);
                } else if !j.new_in_phase && j.state == SlotState::Wait {
                    // a job held in red runs again in yellow
                    set(&mut d, j, SlotState::Go);
                }
            }
        }
        Zone::Red => {
            if s.relief_ineffective {
                d.cache_target = Some(u64::MAX); // R9B-02, as in yellow
            } else if s.cache_used > 0 {
                d.cache_target = Some(s.cache_used - (s.level.saturating_sub(s.yellow_at)).min(s.cache_used));
            }
            for j in jobs {
                if Some(j.id) == oldest {
                    if j.state == SlotState::Wait {
                        set(&mut d, j, SlotState::Go);
                    }
                } else if j.state != SlotState::Probe {
                    set(&mut d, j, SlotState::Wait);
                }
            }
            d.over_limit_single = s.level > s.limit && jobs.iter().filter(|j| j.state != SlotState::Cancel).count() >= 1;
            if d.over_limit_single && s.cancel_over_limit && s.over_limit_ticks >= CANCEL_AFTER_TICKS {
                if let Some(j) = jobs.iter().find(|j| Some(j.id) == oldest && j.grown_since_over > 0) {
                    d.signals.retain(|(i, _)| *i != j.id);
                    d.signals.push((j.id, SlotState::Cancel));
                }
            }
        }
    }
    // liveness: never all registered jobs held (yellow can hold every new
    // job when the old ones ended)
    let after = |id: u64| {
        let pending = d.signals.iter().rev().find(|(i, _)| *i == id).map(|x| x.1);
        pending.unwrap_or_else(|| jobs.iter().find(|j| j.id == id).unwrap().state)
    };
    let runs = |id: u64| !matches!(after(id), SlotState::Wait | SlotState::Cancel);
    // the oldest job that is not (being) cancelled: after a CANCEL of N5
    // the next one runs
    let alive = jobs.iter().filter(|j| after(j.id) != SlotState::Cancel).map(|j| j.id).min();
    if let Some(o) = alive {
        if !jobs.iter().any(|j| runs(j.id)) {
            d.signals.retain(|(i, _)| *i != o);
            if jobs.iter().find(|j| j.id == o).unwrap().state == SlotState::Wait {
                d.signals.push((o, SlotState::Go));
            }
        }
    }
    d
}
