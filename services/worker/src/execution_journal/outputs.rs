use super::*;
impl ExecutionJournal {
    pub(crate) fn prepare_output(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
        h: &worker::OutputHeader,
        digest: &str,
    ) -> Result<(), ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.state != "running" || record.assignment.session_id != identity.session_id {
            return Err(ExecutionJournalError::Stale);
        }
        let ready:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM cpu_runs WHERE command_id=? AND phase='computed_unsubmitted' AND cleanup_confirmed=1 AND response_digest=?)",params![command,h.response_digest],|r|r.get(0))?;
        if !ready {
            return Err(ExecutionJournalError::Stale);
        }
        self.connection.execute("INSERT INTO execution_outputs(command_id,header,header_digest,state) VALUES(?,?,?,'prepared')",params![command,h.encode_to_vec(),digest])?;
        Ok(())
    }
    pub(crate) fn output_header(
        &self,
        command: &str,
    ) -> Result<Option<(worker::OutputHeader, String)>, ExecutionJournalError> {
        let row: Option<(Vec<u8>, String)> = self
            .connection
            .query_row(
                "SELECT header,header_digest FROM execution_outputs WHERE command_id=?",
                [command],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(bytes, digest)| {
            Ok((
                worker::OutputHeader::decode(bytes.as_slice())
                    .map_err(|_| ExecutionJournalError::Format)?,
                digest,
            ))
        })
        .transpose()
    }
    pub fn unresolved_outputs(&self) -> Result<Vec<String>, ExecutionJournalError> {
        let mut statement=self.connection.prepare("SELECT command_id FROM execution_outputs o WHERE state!='confirmed' AND NOT EXISTS(SELECT 1 FROM cpu_cleanup_facts c WHERE c.command_id=o.command_id AND c.receipt IS NOT NULL) ORDER BY command_id LIMIT 16")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub(crate) fn confirm_output(
        &mut self,
        command: &str,
        r: &worker::OutputReceipt,
    ) -> Result<(), ExecutionJournalError> {
        let bytes = r.encode_to_vec();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<Vec<u8>> = tx.query_row(
            "SELECT receipt FROM execution_outputs WHERE command_id=?",
            [command],
            |row| row.get(0),
        )?;
        if prior.as_ref().is_some_and(|p| p != &bytes) {
            return Err(ExecutionJournalError::Invalid);
        }
        if tx.execute("UPDATE execution_outputs SET state='confirmed',receipt=? WHERE command_id=? AND header_digest=?",params![bytes,command,r.header_digest])?!=1 {return Err(ExecutionJournalError::Stale);}
        tx.commit()?;
        Ok(())
    }
}
