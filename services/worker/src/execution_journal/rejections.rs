use super::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionKind {
    Resource,
    Protocol,
}
impl RejectionKind {
    pub(crate) fn wire(self) -> i32 {
        match self {
            Self::Resource => worker::AckResult::ResourceRejected as i32,
            Self::Protocol => worker::AckResult::ProtocolRejected as i32,
        }
    }
    fn decode(value: i32) -> Result<Self, ExecutionJournalError> {
        match worker::AckResult::try_from(value) {
            Ok(worker::AckResult::ResourceRejected) => Ok(Self::Resource),
            Ok(worker::AckResult::ProtocolRejected) => Ok(Self::Protocol),
            _ => Err(ExecutionJournalError::Format),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectionView {
    pub kind: RejectionKind,
    /// Some only after an authenticated cloud receipt was committed locally.
    pub receipt_id: Option<String>,
}
fn matching(receipt: &worker::RejectionReceipt, ack: &worker::Acknowledge) -> bool {
    valid_id(&receipt.receipt_id)
        && receipt.command_id == ack.command_id
        && receipt.payload_digest == ack.payload_digest
        && receipt.lease == ack.lease
        && receipt.result == ack.result
}
impl ExecutionJournal {
    pub(super) fn rejection_view(
        &self,
        command: &str,
        a: &worker::Assignment,
    ) -> Result<Option<RejectionView>, ExecutionJournalError> {
        let row: Option<(i32, Vec<u8>, Option<Vec<u8>>)> = self
            .connection
            .query_row(
                "SELECT result,ack,receipt FROM execution_rejections WHERE command_id=?",
                [command],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(result, bytes, receipt)| {
            let kind = RejectionKind::decode(result)?;
            let ack = worker::Acknowledge::decode(bytes.as_slice())
                .map_err(|_| ExecutionJournalError::Format)?;
            if ack.result != result || acknowledge_digest(&ack, a).is_none() {
                return Err(ExecutionJournalError::Format);
            }
            let receipt = receipt
                .map(|bytes| {
                    worker::RejectionReceipt::decode(bytes.as_slice())
                        .map_err(|_| ExecutionJournalError::Format)
                })
                .transpose()?;
            if receipt.as_ref().is_some_and(|r| !matching(r, &ack)) {
                return Err(ExecutionJournalError::Format);
            }
            Ok(RejectionView {
                kind,
                receipt_id: receipt.map(|r| r.receipt_id),
            })
        })
        .transpose()
    }
    pub(crate) fn reject(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
        kind: RejectionKind,
    ) -> Result<RejectionView, ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.state != "received"
            || record.grant.is_some()
            || record.assignment.session_id != identity.session_id
        {
            return Err(ExecutionJournalError::Stale);
        }
        let ack = worker::Acknowledge {
            command_id: record.command_id.clone(),
            payload_digest: record.assignment.payload_digest.clone(),
            lease: record.assignment.lease.clone(),
            result: kind.wire(),
            execution_journal_id: String::new(),
            broker_grant_id: String::new(),
            broker_epoch: 0,
            slot_generation: 0,
        };
        acknowledge_digest(&ack, &record.assignment).ok_or(ExecutionJournalError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO execution_rejections(command_id,result,ack) VALUES(?,?,?)",
            params![command, ack.result, ack.encode_to_vec()],
        )?;
        tx.execute(
            "UPDATE execution_commands SET state='rejected' WHERE command_id=?",
            [command],
        )?;
        tx.commit()?;
        Ok(RejectionView {
            kind,
            receipt_id: None,
        })
    }
    pub(crate) fn rejected_ack(
        &self,
        identity: &ControlIdentity,
        command: &str,
    ) -> Result<worker::Acknowledge, ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.state != "rejected" || record.grant.is_some() || record.rejection.is_none() {
            return Err(ExecutionJournalError::Stale);
        }
        let bytes: Vec<u8> = self.connection.query_row(
            "SELECT ack FROM execution_rejections WHERE command_id=?",
            [command],
            |r| r.get(0),
        )?;
        worker::Acknowledge::decode(bytes.as_slice()).map_err(|_| ExecutionJournalError::Format)
    }
    pub(crate) fn confirm_rejection(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
        receipt: &worker::RejectionReceipt,
    ) -> Result<RejectionView, ExecutionJournalError> {
        let ack = self.rejected_ack(identity, command)?;
        if !matching(receipt, &ack) {
            return Err(ExecutionJournalError::Invalid);
        }
        let bytes = receipt.encode_to_vec();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<Vec<u8>> = tx.query_row(
            "SELECT receipt FROM execution_rejections WHERE command_id=?",
            [command],
            |r| r.get(0),
        )?;
        if old.as_ref().is_some_and(|prior| prior != &bytes) {
            return Err(ExecutionJournalError::Invalid);
        }
        if old.is_none() {
            tx.execute(
                "UPDATE execution_rejections SET receipt=? WHERE command_id=?",
                params![bytes, command],
            )?;
        }
        tx.commit()?;
        Ok(RejectionView {
            kind: RejectionKind::decode(ack.result)?,
            receipt_id: Some(receipt.receipt_id.clone()),
        })
    }
}
