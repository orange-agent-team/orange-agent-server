use super::ReportError;
use super::events::{CapacityState, Key};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Change {
    eligible: i64,
    excluded: i64,
    paid: i64,
}
type Timeline = BTreeMap<u64, Change>;
#[derive(Default)]
pub(super) struct Supply {
    nodes: BTreeMap<Key, u64>,
    slots: BTreeMap<(Key, Key), Timeline>,
}
pub(super) struct Totals {
    pub registered: usize,
    pub effective: usize,
    pub available: u128,
    pub paid: u128,
    pub paid_outside: u128,
}
impl Supply {
    pub fn registered(&self, node: &Key, at: u64) -> bool {
        self.nodes.get(node).is_some_and(|t| *t <= at)
    }
    pub fn register(&mut self, node: &Key, at: u64) -> Result<(), ReportError> {
        if self.nodes.insert(node.clone(), at).is_some() {
            return Err(ReportError("duplicate node registration"));
        }
        Ok(())
    }
    pub fn interval(
        &mut self,
        node: &Key,
        slot: &Key,
        start: u64,
        end: u64,
        state: Option<CapacityState>,
    ) -> Result<(), ReportError> {
        if end <= start || !self.registered(node, start) {
            return Err(ReportError("invalid node interval"));
        }
        let line = self.slots.entry((node.clone(), slot.clone())).or_default();
        for (t, delta) in [(start, 1), (end, -1)] {
            let change = line.entry(t).or_default();
            match state {
                Some(CapacityState::Eligible) => change.eligible += delta,
                Some(_) => change.excluded += delta,
                None => change.paid += delta,
            }
        }
        Ok(())
    }
    pub fn totals(&self, from: u64, until: u64) -> Result<Totals, ReportError> {
        let mut effective = BTreeSet::new();
        let mut available = 0;
        let mut paid = 0;
        let mut paid_outside = 0;
        for ((node, _), line) in &self.slots {
            let mut previous = 0;
            let mut active = Change::default();
            for (&time, delta) in line {
                let seconds = u128::from(time.min(until).saturating_sub(previous.max(from)));
                if active.paid > 1 && seconds > 0 {
                    return Err(ReportError("overlapping settled attempts on one slot"));
                }
                if active.eligible > 0 && active.excluded == 0 && seconds > 0 {
                    available += seconds;
                    effective.insert(node);
                    if active.paid == 1 {
                        paid += seconds;
                    }
                }
                if (active.eligible == 0 || active.excluded > 0) && active.paid == 1 {
                    paid_outside += seconds;
                }
                active.eligible += delta.eligible;
                active.excluded += delta.excluded;
                active.paid += delta.paid;
                previous = time;
            }
        }
        Ok(Totals {
            registered: self.nodes.values().filter(|&&at| at < until).count(),
            effective: effective.len(),
            available,
            paid,
            paid_outside,
        })
    }
}
