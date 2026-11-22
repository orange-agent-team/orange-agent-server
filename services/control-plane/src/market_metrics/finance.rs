use super::{ReportError, Result, events::*};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Cash {
    captured: i128,
    refunded: i128,
    chargebacks: i128,
    recoveries: i128,
    service: i128,
    cost: BTreeMap<CostKind, i128>,
    coverage: BTreeSet<CostKind>,
}
struct Lot<'a> {
    payer: &'a Key,
    currency: &'a Currency,
    minor: i128,
    units: i128,
    used: i128,
    refunded: i128,
    returned: i128,
    disputed: i128,
    at: u64,
}
struct Dispute<'a> {
    order: &'a Key,
    remaining: i128,
}
#[derive(Default)]
struct Credits {
    charge: i128,
    gift: i128,
    earned: i128,
    purchased: i128,
    provider: i128,
    fee: i128,
    unattributed: i128,
}
#[derive(Default)]
pub(super) struct Book<'a> {
    cash: BTreeMap<&'a Currency, Cash>,
    lots: BTreeMap<&'a Key, Lot<'a>>,
    refunds: BTreeSet<&'a Key>,
    disputes: BTreeMap<&'a Key, Dispute<'a>>,
    credits: Credits,
    consumers: BTreeSet<&'a Key>,
}
pub(super) struct Report {
    pub consumers: usize,
    pub buyers: usize,
    pub repeat: usize,
    pub credits: Value,
    pub currency_totals: Vec<Value>,
}
impl<'a> Book<'a> {
    pub fn purchase(&mut self, p: &'a Purchase, at: u64, period: bool) -> Result<()> {
        let Purchase {
            order,
            payer,
            currency,
            minor,
            units,
        } = p;
        let lot = Lot {
            payer,
            currency,
            minor: minor.0.into(),
            units: units.0.into(),
            used: 0,
            refunded: 0,
            returned: 0,
            disputed: 0,
            at,
        };
        if minor.0 == 0 || units.0 == 0 || self.lots.insert(order, lot).is_some() {
            return Err(ReportError("invalid payment lot"));
        }
        if period {
            self.cash.entry(currency).or_default().captured += i128::from(minor.0);
        }
        Ok(())
    }
    pub fn refund(
        &mut self,
        id: &'a Key,
        order: &Key,
        minor: Units,
        units: Units,
        period: bool,
    ) -> Result<()> {
        let lot = self
            .lots
            .get_mut(order)
            .ok_or(ReportError("refund lacks original payment"))?;
        if !self.refunds.insert(id) || minor.0 == 0 || units.0 == 0 {
            return Err(ReportError("invalid refund receipt"));
        }
        let returned = lot.returned + i128::from(minor.0);
        if returned + lot.disputed > lot.minor {
            return Err(ReportError("refund exceeds remaining cash"));
        }
        // Same cumulative ceiling as the original purchase refund contract.
        let cumulative = (returned * lot.units + lot.minor - 1) / lot.minor;
        if cumulative - lot.refunded != i128::from(units.0) || lot.used + cumulative > lot.units {
            return Err(ReportError(
                "refund differs from original-price credit hold",
            ));
        }
        lot.returned = returned;
        lot.refunded = cumulative;
        if period {
            self.cash.entry(lot.currency).or_default().refunded += i128::from(minor.0);
        }
        Ok(())
    }
    pub fn chargeback(
        &mut self,
        key: &'a Key,
        order: &'a Key,
        value: Units,
        period: bool,
    ) -> Result<()> {
        let lot = self
            .lots
            .get_mut(order)
            .ok_or(ReportError("chargeback lacks payment"))?;
        let minor = i128::from(value.0);
        if minor == 0
            || lot.disputed + lot.returned + minor > lot.minor
            || self
                .disputes
                .insert(
                    key,
                    Dispute {
                        order,
                        remaining: minor,
                    },
                )
                .is_some()
        {
            return Err(ReportError("invalid chargeback"));
        }
        lot.disputed += minor;
        if period {
            self.cash.entry(lot.currency).or_default().chargebacks += minor;
        }
        Ok(())
    }
    pub fn recover(&mut self, key: &Key, value: Units, period: bool) -> Result<()> {
        let dispute = self
            .disputes
            .get_mut(key)
            .ok_or(ReportError("unknown chargeback"))?;
        let amount = i128::from(value.0);
        if amount == 0 || amount > dispute.remaining {
            return Err(ReportError("invalid dispute recovery"));
        }
        let lot = self
            .lots
            .get_mut(dispute.order)
            .ok_or(ReportError("unknown original payment"))?;
        dispute.remaining -= amount;
        lot.disputed -= amount;
        if period {
            self.cash.entry(lot.currency).or_default().recoveries += amount;
        }
        Ok(())
    }
    pub fn expense(&mut self, currency: &'a Currency, category: CostKind, value: Units) {
        *self
            .cash
            .entry(currency)
            .or_default()
            .cost
            .entry(category)
            .or_default() += i128::from(value.0);
    }
    pub fn settle(&mut self, payer: &'a Key, terms: &'a Settlement, period: bool) -> Result<()> {
        let amount = i128::from(terms.charge.0);
        if amount
            != i128::from(terms.gift.0) + i128::from(terms.earned.0) + i128::from(terms.purchased.0)
            || amount != i128::from(terms.provider_pending.0) + i128::from(terms.platform_fee.0)
        {
            return Err(ReportError("unbalanced settlement"));
        }
        let mut attributed = 0;
        let mut unique = BTreeSet::new();
        for part in &terms.lots {
            let lot = self
                .lots
                .get_mut(&part.order)
                .ok_or(ReportError("missing purchased lot"))?;
            if !unique.insert(&part.order) || part.units.0 == 0 || lot.payer != payer {
                return Err(ReportError("invalid purchase attribution"));
            }
            let before = lot.used;
            lot.used += i128::from(part.units.0);
            if lot.used + lot.refunded > lot.units {
                return Err(ReportError("purchase lot overspent"));
            }
            attributed += i128::from(part.units.0);
            if period {
                self.cash.entry(lot.currency).or_default().service +=
                    lot.used * lot.minor / lot.units - before * lot.minor / lot.units;
            }
        }
        if attributed > i128::from(terms.purchased.0) {
            return Err(ReportError("attribution exceeds purchased charge"));
        }
        if period {
            if terms.purchased.0 > 0 {
                self.consumers.insert(payer);
            }
            self.credits.charge += amount;
            self.credits.gift += i128::from(terms.gift.0);
            self.credits.earned += i128::from(terms.earned.0);
            self.credits.purchased += i128::from(terms.purchased.0);
            self.credits.provider += i128::from(terms.provider_pending.0);
            self.credits.fee += i128::from(terms.platform_fee.0);
            self.credits.unattributed += i128::from(terms.purchased.0) - attributed;
        }
        Ok(())
    }
    pub fn finish(
        mut self,
        coverage: &'a [Coverage],
        from: u64,
        until: u64,
        settled: usize,
    ) -> Result<Report> {
        let mut histories: BTreeMap<&Key, (usize, bool)> = BTreeMap::new();
        let mut dispute_balance: BTreeMap<&Currency, i128> = BTreeMap::new();
        for lot in self.lots.values() {
            if lot.disputed > 0 {
                *dispute_balance.entry(lot.currency).or_default() += lot.disputed;
            }
            if lot.at < until && lot.returned + lot.disputed < lot.minor {
                let history = histories.entry(lot.payer).or_default();
                history.0 += 1;
                history.1 |= lot.at >= from;
            }
        }
        let buyers = histories.values().filter(|(_, current)| *current).count();
        let repeat = histories
            .values()
            .filter(|(count, current)| *current && *count > 1)
            .count();
        let mut declared = BTreeSet::new();
        let mut receipts = BTreeSet::new();
        for row in coverage {
            let metrics = self.cash.entry(&row.currency).or_default();
            if !declared.insert(&row.currency) || !receipts.insert(&row.receipt) {
                return Err(ReportError("duplicate cost coverage"));
            }
            for k in &row.complete {
                if !metrics.coverage.insert(*k) {
                    return Err(ReportError("duplicate covered cost category"));
                }
            }
        }
        for currency in dispute_balance.keys() {
            self.cash.entry(currency).or_default();
        }
        let c = self.credits;
        let currency_totals=self.cash.into_iter().map(|(currency,m)|{
            let open=dispute_balance.get(currency).copied().unwrap_or(0);
            let cost:i128=m.cost.values().sum();
            let costs_complete=COST_KINDS.iter().all(|k|m.coverage.contains(k));
            let margin=(costs_complete && c.unattributed==0 && open==0).then_some(m.service-cost);
            let components:BTreeMap<_,_>=COST_KINDS.iter().map(|k|(*k,m.cost.get(k).map(ToString::to_string).or_else(||m.coverage.contains(k).then(||"0".into())))).collect();
            json!({"currency":currency,"purchase_minor":m.captured.to_string(),"refund_minor":m.refunded.to_string(),
                "chargeback_minor":m.chargebacks.to_string(),"recovered_dispute_minor":m.recoveries.to_string(),"open_dispute_minor_as_of":open.to_string(),
                "net_receipts_minor":(m.captured-m.refunded-m.chargebacks+m.recoveries).to_string(),
                "attributed_service_minor":m.service.to_string(),"costs_minor":components,"complete_cost_categories":m.coverage,"known_cost_minor":cost.to_string(),
                "contribution_minor":margin.map(|v|v.to_string()),"contribution_rate":margin.filter(|_|m.service>0).map(|v|json!({"numerator":v.to_string(),"denominator":m.service.to_string()})),
                "cost_per_settled_job":(costs_complete&&settled>0).then(||json!({"numerator_minor":cost.to_string(),"denominator_jobs":settled}))})
        }).collect();
        Ok(Report {
            consumers: self.consumers.len(),
            buyers,
            repeat,
            currency_totals,
            credits: json!({"charged":c.charge.to_string(),"gift_used":c.gift.to_string(),"earned_used":c.earned.to_string(),"purchased_used":c.purchased.to_string(),"unattributed_purchased":c.unattributed.to_string(),"provider_pending":c.provider.to_string(),"platform_fee":c.fee.to_string()}),
        })
    }
}
