//! Reconcile a bounded, as-of extract of pseudonymous operational and financial facts.
mod events;
mod finance;
pub mod keys;
mod supply;
pub use events::Snapshot;
use events::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReportError(&'static str);
type Result<T> = std::result::Result<T, ReportError>;

struct Attempt<'a> {
    job: &'a Key,
    node: &'a Key,
    slot: &'a Key,
    start: u64,
    end: u64,
}
struct Job<'a> {
    payer: &'a Key,
    finished: Option<(Outcome, Option<&'a Key>)>,
    settled: Option<(u64, i64)>,
}
fn ratio(n: u128, d: u128) -> Value {
    if d == 0 {
        Value::Null
    } else {
        json!({"numerator":n.to_string(),"denominator":d.to_string()})
    }
}
fn priority(f: &Fact) -> u8 {
    match f {
        Fact::NodeRegistered { .. } => 0,
        Fact::Purchase(_) => 1,
        Fact::JobSubmitted { .. } => 2,
        Fact::Capacity { .. } | Fact::Attempt { .. } => 3,
        Fact::JobFinished { .. } => 4,
        Fact::Settlement(_) => 5,
        Fact::Refund { .. } => 6,
        Fact::Chargeback { .. } => 7,
        Fact::ChargebackRecovery { .. } => 8,
        Fact::Expense { .. } => 9,
        Fact::EarningReleased { .. } => 10,
    }
}

pub fn report(s: &Snapshot) -> Result<Value> {
    if s.version != 1
        || s.until_unix <= s.from_unix
        || s.as_of_unix < s.until_unix
        || (s.population == Population::SettledPayerCohort && s.supply_history_complete)
        || s.events.len() > 1_000_000
    {
        return Err(ReportError("invalid report version, window or size"));
    }
    let inside = |t: u64| s.from_unix <= t && t < s.until_unix;
    let mut seen = BTreeMap::new();
    let mut events = Vec::new();
    for e in &s.events {
        if e.at > s.as_of_unix {
            return Err(ReportError("event later than as-of cutoff"));
        }
        if let Some(old) = seen.insert(&e.receipt, e) {
            if old != e {
                return Err(ReportError("conflicting receipt"));
            }
            continue;
        }
        events.push(e);
    }
    events.sort_unstable_by_key(|e| (e.at, priority(&e.fact), &e.receipt));
    let mut supply = supply::Supply::default();
    let mut book = finance::Book::default();
    let mut releases = BTreeSet::new();
    let mut released = 0i128;
    let mut jobs = BTreeMap::new();
    let mut attempts = BTreeMap::new();

    let (mut submitted, mut delivered, mut failed, mut cancelled, mut settled, mut attempt_count) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut delivered_nodes = BTreeSet::new();

    for e in events {
        let at = e.at;
        let period = inside(at);
        if !s.supply_history_complete
            && matches!(
                e.fact,
                Fact::Attempt { .. } | Fact::NodeRegistered { .. } | Fact::Capacity { .. }
            )
        {
            return Err(ReportError(
                "incomplete supply extract must omit node/slot facts",
            ));
        }
        match &e.fact {
            Fact::NodeRegistered { node } => supply.register(node, at)?,
            Fact::Capacity {
                node,
                slot,
                until,
                state,
            } => {
                if *until > s.as_of_unix {
                    return Err(ReportError("capacity exceeds observation cutoff"));
                }
                supply.interval(node, slot, at, *until, Some(*state))?;
            }
            Fact::JobSubmitted { job, payer } => {
                if jobs
                    .insert(
                        job,
                        Job {
                            payer,
                            finished: None,
                            settled: None,
                        },
                    )
                    .is_some()
                {
                    return Err(ReportError("duplicate submitted job"));
                }
                submitted += usize::from(period);
            }
            Fact::Attempt {
                attempt,
                job,
                node,
                slot,
                until,
            } => {
                if !supply.registered(node, at)
                    || !jobs.get(job).is_some_and(|j| j.finished.is_none())
                    || *until <= at
                    || *until > s.as_of_unix
                    || attempts
                        .insert(
                            attempt,
                            Attempt {
                                job,
                                node,
                                slot,
                                start: at,
                                end: *until,
                            },
                        )
                        .is_some()
                {
                    return Err(ReportError("invalid execution attempt"));
                }
                attempt_count += usize::from(period);
            }
            Fact::JobFinished {
                job,
                outcome,
                winner,
            } => {
                let j = jobs
                    .get_mut(job)
                    .ok_or(ReportError("missing original job"))?;
                if j.finished.is_some() {
                    return Err(ReportError("duplicate job outcome"));
                }
                match (outcome, winner) {
                    (Outcome::Delivered, Some(id)) => {
                        let a = attempts
                            .get(id)
                            .ok_or(ReportError("missing winning attempt"))?;
                        if a.job != job || a.end > at {
                            return Err(ReportError("wrong winning attempt"));
                        }
                        if period {
                            delivered += 1;
                            delivered_nodes.insert(a.node);
                        }
                    }
                    (Outcome::Delivered, None) if !s.supply_history_complete => {
                        delivered += usize::from(period)
                    }
                    (Outcome::Failed, None) => failed += usize::from(period),
                    (Outcome::Cancelled, None) => cancelled += usize::from(period),
                    _ => return Err(ReportError("inconsistent delivery outcome")),
                }
                j.finished = Some((*outcome, winner.as_ref()));
            }
            Fact::Settlement(terms) => {
                let j = jobs
                    .get_mut(&terms.job)
                    .ok_or(ReportError("missing settled job"))?;
                let Some((Outcome::Delivered, winner)) = j.finished else {
                    return Err(ReportError("unconfirmed job was charged"));
                };
                if j.settled.is_some() {
                    return Err(ReportError("duplicate settlement"));
                }
                book.settle(j.payer, terms, period)?;
                j.settled = Some((at, terms.provider_pending.0));
                if let Some(id) = winner
                    && terms.charge.0 > 0
                {
                    let a = &attempts[id];
                    supply.interval(a.node, a.slot, a.start, a.end, None)?;
                }
                settled += usize::from(period);
            }
            Fact::EarningReleased { job, units } => {
                let origin = jobs
                    .get(job)
                    .and_then(|j| j.settled)
                    .ok_or(ReportError("provider release lacks original settlement"))?;
                if !s.earning_releases_complete
                    || units.0 <= 0
                    || units.0 != origin.1
                    || at.saturating_sub(origin.0) < 86400
                    || !releases.insert(job)
                {
                    return Err(ReportError("invalid provider release"));
                }
                if period {
                    released += i128::from(units.0);
                }
            }
            Fact::Purchase(p) => book.purchase(p, at, period)?,
            Fact::Refund {
                refund,
                order,
                minor,
                units,
            } => book.refund(refund, order, *minor, *units, period)?,
            Fact::Chargeback {
                dispute,
                order,
                minor,
            } => book.chargeback(dispute, order, *minor, period)?,
            Fact::ChargebackRecovery { dispute, minor } => book.recover(dispute, *minor, period)?,
            Fact::Expense {
                currency,
                category,
                minor,
            } => {
                if period {
                    book.expense(currency, *category, *minor);
                }
            }
        }
    }
    let mut accounts = book.finish(&s.cost_coverage, s.from_unix, s.until_unix, settled)?;
    accounts.credits["provider_released"] =
        json!(s.earning_releases_complete.then(|| released.to_string()));
    let n = supply.totals(s.from_unix, s.until_unix)?;
    let supply_totals=s.supply_history_complete.then(||json!({"registered_nodes":n.registered,"effective_nodes":n.effective,"completed_nodes":delivered_nodes.len(),"eligible_slot_seconds":n.available.to_string(),"paid_slot_seconds":n.paid.to_string(),"paid_outside_eligible_seconds":n.paid_outside.to_string(),"utilization":(n.paid_outside==0).then(||ratio(n.paid,n.available))}));
    Ok(
        json!({"version":1,"from_unix":s.from_unix,"until_unix":s.until_unix,"as_of_unix":s.as_of_unix,"population":s.population,
        "supply":supply_totals,
        "demand":{"submitted":(s.population==Population::ActivityExtract).then_some(submitted),"delivered":(s.population==Population::ActivityExtract).then_some(delivered),"failed":(s.population==Population::ActivityExtract).then_some(failed),"cancelled":(s.population==Population::ActivityExtract).then_some(cancelled),"settled":settled,"attempts":s.supply_history_complete.then_some(attempt_count),"purchased_credit_consumers":accounts.consumers,"purchasers":accounts.buyers,"repeat_purchasers":accounts.repeat,"repurchase_rate":ratio(accounts.repeat as u128,accounts.buyers as u128)},
        "microcredits":accounts.credits,"currency_totals":accounts.currency_totals}),
    )
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn committed_sample_and_ingestor_respect_the_published_closed_event_schema() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../contracts/json-schema/market-metrics.snapshot.v1.json"
        ))
        .expect("contract");
        let validator = jsonschema::validator_for(&schema).expect("local-only schema");
        let mut sample: Value = serde_json::from_str(include_str!(
            "../../../../docs/market-metrics/sample.snapshot.json"
        ))
        .expect("sample");
        assert!(validator.is_valid(&sample));
        let expected: Value = serde_json::from_str(include_str!(
            "../../../../docs/market-metrics/sample.report.json"
        ))
        .expect("expected aggregate");
        let parsed: Snapshot = serde_json::from_value(sample.clone()).expect("typed input");
        assert_eq!(report(&parsed).expect("report"), expected);
        for event in sample["events"].as_array().expect("events") {
            let mut private = event.clone();
            private["fact"]["prompt"] = json!("PRIVATE_sample");
            let mut invalid = sample.clone();
            invalid["events"] = json!([private]);
            assert!(!validator.is_valid(&invalid));
            assert!(serde_json::from_value::<Snapshot>(invalid).is_err());
        }
        sample["population"] = json!("settled_payer_cohort");
        assert!(!validator.is_valid(&sample));
        assert!(report(&serde_json::from_value(sample).expect("unqualified scope")).is_err());
    }
}
