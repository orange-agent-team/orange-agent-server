//! Pure, checked rules. Storage, authorization and concurrency belong to callers.
pub mod identity;

use std::{error::Error, fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainError {
    InvalidAmount,
    Overflow,
    InsufficientBalance,
    BudgetExceeded,
    BudgetClosing,
    InvalidTransition,
    ReservationMismatch,
    InvalidFee,
    AttemptLimit,
    StaleAuthority,
    EvidenceRequired,
}
impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for DomainError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Credits(i64);
impl Credits {
    pub const ZERO: Self = Self(0);
    pub fn new(value: i64) -> Result<Self, DomainError> {
        if value < 0 {
            Err(DomainError::InvalidAmount)
        } else {
            Ok(Self(value))
        }
    }
    pub fn units(self) -> i64 {
        self.0
    }
    pub fn checked_add(self, rhs: Self) -> Result<Self, DomainError> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(DomainError::Overflow)
    }
    pub fn checked_sub(self, rhs: Self) -> Result<Self, DomainError> {
        self.0
            .checked_sub(rhs.0)
            .filter(|v| *v >= 0)
            .map(Self)
            .ok_or(DomainError::InsufficientBalance)
    }
}
impl fmt::Display for Credits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for Credits {
    type Err = DomainError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty()
            || !s.bytes().all(|c| c.is_ascii_digit())
            || (s.len() > 1 && s.starts_with('0'))
        {
            return Err(DomainError::InvalidAmount);
        }
        Self::new(s.parse().map_err(|_| DomainError::Overflow)?)
    }
}

/// Positive quantities only. u128 intermediate prevents i64 multiplication overflow.
pub fn price_units(units: u64, price: Credits, denominator: u64) -> Result<Credits, DomainError> {
    if denominator == 0 {
        return Err(DomainError::InvalidAmount);
    }
    let product = u128::from(units) * price.units() as u128;
    let cost = product.div_ceil(u128::from(denominator));
    Credits::new(i64::try_from(cost).map_err(|_| DomainError::Overflow)?)
}
pub fn text_price(
    input: u64,
    output: u64,
    input_price: Credits,
    output_price: Credits,
) -> Result<Credits, DomainError> {
    price_units(input, input_price, 1000)?.checked_add(price_units(output, output_price, 1000)?)
}
pub fn split_fee(charge: Credits, basis_points: u16) -> Result<(Credits, Credits), DomainError> {
    if basis_points > 10000 {
        return Err(DomainError::InvalidFee);
    }
    let fee = Credits::new(((charge.units() as i128 * i128::from(basis_points)) / 10000) as i64)?;
    Ok((fee, charge.checked_sub(fee)?))
}

/// Eligibility comes from the host's committed delivery facts, never model output or client claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDelivery {
    CommittedSuccess,
    CancelledPersistedText,
    Uncommitted,
    Failed,
    Superseded,
}
#[derive(Debug, Clone, Copy)]
pub struct TextQuote {
    pub input_units: u64,
    pub output_limit: u64,
    pub input_price: Credits,
    pub output_price: Credits,
}
impl TextQuote {
    pub fn maximum(self) -> Result<Credits, DomainError> {
        text_price(
            self.input_units,
            self.output_limit,
            self.input_price,
            self.output_price,
        )
    }
    /// Pure preview/calculation; this function does not establish delivery, debit funds or settle a Job.
    pub fn charge(
        self,
        delivery: TextDelivery,
        output_units: u64,
        has_tools: bool,
    ) -> Result<Credits, DomainError> {
        if matches!(
            delivery,
            TextDelivery::Uncommitted | TextDelivery::Failed | TextDelivery::Superseded
        ) {
            return Ok(Credits::ZERO);
        }
        if output_units > self.output_limit {
            return Err(DomainError::BudgetExceeded);
        }
        if delivery == TextDelivery::CancelledPersistedText {
            if has_tools {
                return Err(DomainError::EvidenceRequired);
            }
            if output_units == 0 {
                return Ok(Credits::ZERO);
            }
        }
        let charge = text_price(
            self.input_units,
            output_units,
            self.input_price,
            self.output_price,
        )?;
        if charge > self.maximum()? {
            return Err(DomainError::BudgetExceeded);
        }
        Ok(charge)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub limit: Credits,
    pub spent: Credits,
    pub reserved: Credits,
    pub closing: bool,
}
impl Budget {
    pub fn validate(self) -> Result<Self, DomainError> {
        if self.spent.checked_add(self.reserved)? > self.limit {
            return Err(DomainError::BudgetExceeded);
        }
        Ok(self)
    }
    pub fn reserve(self, amount: Credits) -> Result<Self, DomainError> {
        if self.closing {
            return Err(DomainError::BudgetClosing);
        }
        Self {
            reserved: self.reserved.checked_add(amount)?,
            ..self
        }
        .validate()
    }
    /// `held` must be the reservation owned by the transaction. Never guesses it from usage.
    pub fn settle(self, held: Credits, actual: Credits) -> Result<Self, DomainError> {
        if actual > held || held > self.reserved {
            return Err(DomainError::ReservationMismatch);
        }
        Self {
            reserved: self.reserved.checked_sub(held)?,
            spent: self.spent.checked_add(actual)?,
            ..self
        }
        .validate()
    }
    pub fn change_limit(self, limit: Credits) -> Result<Self, DomainError> {
        Self { limit, ..self }.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}
impl JobState {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
    pub fn start(self) -> Result<Self, DomainError> {
        if self == Self::Queued {
            Ok(Self::Running)
        } else {
            Err(DomainError::InvalidTransition)
        }
    }
    pub fn request_cancel(self) -> Result<Self, DomainError> {
        match self {
            Self::Queued => Ok(Self::Cancelled),
            Self::Running | Self::Cancelling => Ok(Self::Cancelling),
            _ => Err(DomainError::InvalidTransition),
        }
    }
    pub fn complete(self) -> Result<Self, DomainError> {
        if self == Self::Running {
            Ok(Self::Succeeded)
        } else {
            Err(DomainError::InvalidTransition)
        }
    }
}
pub fn next_attempt(current_count: u8) -> Result<u8, DomainError> {
    if current_count >= 3 {
        Err(DomainError::AttemptLimit)
    } else {
        Ok(current_count + 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionIdentity {
    pub connection_epoch: u64,
    pub fencing: u64,
    pub attempt: u8,
}
pub fn validate_response(
    current: ExecutionIdentity,
    incoming: ExecutionIdentity,
    complete: bool,
    active: bool,
) -> Result<(), DomainError> {
    if current != incoming {
        return Err(DomainError::StaleAuthority);
    }
    if !complete || !active {
        return Err(DomainError::InvalidTransition);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceGrant {
    pub epoch: u64,
    pub generation: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupEvidence {
    pub task_joined: bool,
    pub processes_exited: bool,
    pub device_healthy: bool,
}
pub fn validate_release(
    current: ResourceGrant,
    reported: ResourceGrant,
    evidence: CleanupEvidence,
) -> Result<(), DomainError> {
    if current != reported {
        return Err(DomainError::StaleAuthority);
    }
    if !(evidence.task_joined && evidence.processes_exited && evidence.device_healthy) {
        return Err(DomainError::EvidenceRequired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(n: i64) -> Credits {
        Credits::new(n).expect("valid fixture")
    }
    #[test]
    fn rejects_noncanonical_and_overflowing_money() {
        for input in [
            "",
            "-1",
            "+1",
            "01",
            "1.0",
            "1e4",
            " 1",
            "9223372036854775808",
        ] {
            assert!(input.parse::<Credits>().is_err(), "{input}");
        }
        assert_eq!("9223372036854775807".parse::<Credits>(), Ok(c(i64::MAX)));
        assert_eq!(c(i64::MAX).checked_add(c(1)), Err(DomainError::Overflow));
        assert_eq!(
            price_units(u64::MAX, c(i64::MAX), 1),
            Err(DomainError::Overflow)
        );
    }
    #[test]
    fn published_billing_examples_and_fee_conservation() {
        assert_eq!(text_price(512, 128, c(2000), c(6000)), Ok(c(1792)));
        assert_eq!(text_price(512, 20, c(2000), c(6000)), Ok(c(1144)));
        assert_eq!(price_units(200, c(1000), 1000), Ok(c(200)));
        assert_eq!(price_units(90000, c(12000), 60000), Ok(c(18000)));
        for amount in 0..1000 {
            for rate in [0, 1, 2000, 9999, 10000] {
                let (fee, provider) = split_fee(c(amount), rate).expect("valid");
                assert_eq!(fee.checked_add(provider), Ok(c(amount)));
            }
        }
        assert_eq!(split_fee(c(1), 10001), Err(DomainError::InvalidFee));
    }
    #[test]
    fn budget_trace_and_closing_preserve_liability() {
        let start = Budget {
            limit: c(10000),
            spent: c(0),
            reserved: c(0),
            closing: false,
        };
        let a = start.reserve(c(6000)).expect("reserve");
        assert!(a.reserve(c(6000)).is_err());
        assert!(a.change_limit(c(5999)).is_err());
        let settled = a.settle(c(6000), c(3500)).expect("settle");
        let b = settled.reserve(c(6000)).expect("reserve b");
        let closed = Budget { closing: true, ..b };
        assert!(closed.reserve(c(1)).is_err());
        assert_eq!(
            closed.settle(c(6000), c(0)).expect("release").spent,
            c(3500)
        );
        assert!(b.settle(c(6001), c(0)).is_err());
        assert!(b.settle(c(6000), c(6001)).is_err());
    }
    #[test]
    fn cancel_wins_before_completion_and_old_authorities_fail() {
        let running = JobState::Queued.start().expect("start");
        assert!(
            running
                .request_cancel()
                .expect("cancel")
                .complete()
                .is_err()
        );
        assert!(
            running
                .complete()
                .expect("complete")
                .request_cancel()
                .is_err()
        );
        assert_eq!(next_attempt(3), Err(DomainError::AttemptLimit));
        let current = ExecutionIdentity {
            connection_epoch: 2,
            fencing: 4,
            attempt: 2,
        };
        assert!(validate_response(current, current, false, true).is_err());
        assert!(validate_response(current, current, true, false).is_err());
        assert!(
            validate_response(
                current,
                ExecutionIdentity {
                    connection_epoch: 1,
                    ..current
                },
                true,
                true
            )
            .is_err()
        );
    }
    #[test]
    fn every_cleanup_fact_is_required_and_old_release_is_rejected() {
        let grant = ResourceGrant {
            epoch: 3,
            generation: 9,
        };
        for mask in 0..8 {
            let evidence = CleanupEvidence {
                task_joined: mask & 1 != 0,
                processes_exited: mask & 2 != 0,
                device_healthy: mask & 4 != 0,
            };
            assert_eq!(validate_release(grant, grant, evidence).is_ok(), mask == 7);
        }
        assert!(
            validate_release(
                grant,
                ResourceGrant {
                    epoch: 3,
                    generation: 8
                },
                CleanupEvidence {
                    task_joined: true,
                    processes_exited: true,
                    device_healthy: true
                }
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod job_transition_matrix {
    use super::*;
    #[test]
    fn every_job_state_accepts_only_its_start_cancel_and_completion_edges() {
        use JobState::*;
        for (state, start, cancel, complete, terminal) in [
            (Queued, Some(Running), Some(Cancelled), None, false),
            (Running, None, Some(Cancelling), Some(Succeeded), false),
            (Cancelling, None, Some(Cancelling), None, false),
            (Succeeded, None, None, None, true),
            (Failed, None, None, None, true),
            (Cancelled, None, None, None, true),
        ] {
            assert_eq!(state.start().ok(), start);
            assert_eq!(state.request_cancel().ok(), cancel);
            assert_eq!(state.complete().ok(), complete);
            assert_eq!(state.terminal(), terminal);
        }
    }
}
