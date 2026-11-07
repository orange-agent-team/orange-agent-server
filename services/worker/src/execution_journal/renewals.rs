use super::*;
impl ExecutionJournal {
    pub(crate) fn next_renewal(
        &self,
        identity: &ControlIdentity,
        command: &str,
    ) -> Result<u64, ExecutionJournalError> {
        self.current(identity)?;
        let blocked:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM execution_renewals WHERE command_id=? AND state!='confirmed')",[command],|r|r.get(0))?;
        if blocked {
            return Err(ExecutionJournalError::Stale);
        }
        let last: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM execution_renewals WHERE command_id=?",
            [command],
            |r| r.get(0),
        )?;
        if last >= 8640 {
            return Err(ExecutionJournalError::Invalid);
        }
        Ok((last + 1) as u64)
    }
    pub(crate) fn begin_renewal(
        &mut self,
        command: &str,
        r: &worker::ExecutionRenewalRequest,
        digest: &str,
    ) -> Result<(), ExecutionJournalError> {
        self.connection.execute("INSERT INTO execution_renewals(command_id,request_id,sequence,request_digest,state) VALUES(?,?,?,?,'pending')",params![command,r.request_id,r.sequence as i64,digest])?;
        Ok(())
    }
    pub(crate) fn confirm_renewal(
        &mut self,
        r: &worker::ExecutionRenewalRequest,
        reply: &worker::ExecutionRenewalReply,
    ) -> Result<(), ExecutionJournalError> {
        let expires = orange_protocol::execution::seconds(&reply.expires_at)
            .ok_or(ExecutionJournalError::Invalid)?;
        if self.connection.execute("UPDATE execution_renewals SET state='confirmed',budget_ms=?,expires_seconds=? WHERE request_id=? AND sequence=? AND request_digest=? AND state='pending'",params![reply.budget_ms as i64,expires,r.request_id,r.sequence as i64,reply.request_digest])?!=1 {return Err(ExecutionJournalError::Stale);}
        Ok(())
    }
}
