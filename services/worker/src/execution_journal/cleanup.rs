use super::*;
use orange_protocol::cleanup::CpuCleanupEvidence;
impl ExecutionJournal {
    #[cfg(unix)]
    pub(crate) fn record_cpu_cleanup(
        &mut self,
        command: &str,
        grant: &Grant,
        process: &orange_model_runtime::supervisor::ProcessRecord,
        digest: &str,
    ) -> Result<(), ExecutionJournalError> {
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.grant.as_ref() != Some(grant) {
            return Err(ExecutionJournalError::Stale);
        }
        let matches:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM cpu_runs WHERE command_id=? AND instance_id=? AND cleanup_confirmed=1 AND cleanup_digest=?)",params![command,process.instance_id,digest],|r|r.get(0))?;
        if !matches {
            return Err(ExecutionJournalError::Stale);
        }
        let e:CpuCleanupEvidence=serde_json::from_value(serde_json::json!({"schema_version":1,"grant":grant,"process":process,"local_cleanup_digest":digest})).map_err(|_|ExecutionJournalError::Invalid)?;
        if !e.valid() {
            return Err(ExecutionJournalError::Invalid);
        }
        self.connection.execute(
            "INSERT INTO cpu_cleanup_facts(command_id,model_instance_id,evidence) VALUES(?,?,?)",
            params![
                command,
                process.instance_id,
                serde_json::to_vec(&e).map_err(|_| ExecutionJournalError::Invalid)?
            ],
        )?;
        Ok(())
    }
    pub fn pending_cleanups(&self) -> Result<Vec<String>, ExecutionJournalError> {
        let mut statement=self.connection.prepare("SELECT command_id FROM cpu_cleanup_facts WHERE receipt IS NULL ORDER BY command_id LIMIT 16")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub(crate) fn cleanup_request(
        &mut self,
        command: &str,
        session: &str,
    ) -> Result<worker::CpuCleanupRequest, ExecutionJournalError> {
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        let (bytes,phase):(Vec<u8>,String)=self.connection.query_row("SELECT f.evidence,r.phase FROM cpu_cleanup_facts f JOIN cpu_runs r USING(command_id) WHERE command_id=?",[command],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let e = CpuCleanupEvidence::parse(&bytes).ok_or(ExecutionJournalError::Format)?;
        let prior: Option<Vec<u8>> = self.connection.query_row(
            "SELECT request FROM cpu_cleanup_facts WHERE command_id=?",
            [command],
            |r| r.get(0),
        )?;
        if let Some(prior) = prior {
            let mut request = worker::CpuCleanupRequest::decode(prior.as_slice())
                .map_err(|_| ExecutionJournalError::Format)?;
            if request.accepted.as_ref() != Some(&record.ack()?)
                || request.evidence_json != bytes
                || orange_protocol::cleanup::cleanup_digest(
                    request
                        .accepted
                        .as_ref()
                        .ok_or(ExecutionJournalError::Format)?,
                    &record.assignment,
                    &e,
                    &request.outcome,
                )
                .as_deref()
                    != Some(request.request_digest.as_str())
            {
                return Err(ExecutionJournalError::Format);
            }
            request.session_id = session.into();
            return Ok(request);
        }
        let outcome = if phase == "computed_unsubmitted" {
            let confirmed:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM execution_outputs WHERE command_id=? AND state='confirmed')",[command],|r|r.get(0))?;
            if confirmed {
                "delivered"
            } else {
                "delivery_unknown"
            }
        } else {
            phase.as_str()
        };
        let ack = record.ack()?;
        let digest =
            orange_protocol::cleanup::cleanup_digest(&ack, &record.assignment, &e, outcome)
                .ok_or(ExecutionJournalError::Invalid)?;
        let request = worker::CpuCleanupRequest {
            session_id: session.into(),
            accepted: Some(ack),
            evidence_json: bytes,
            outcome: outcome.into(),
            request_digest: digest,
        };
        if self.connection.execute("UPDATE cpu_cleanup_facts SET request=?,request_digest=? WHERE command_id=? AND request IS NULL",params![request.encode_to_vec(),request.request_digest,command])?!=1 {return Err(ExecutionJournalError::Stale);}
        Ok(request)
    }
    pub(crate) fn confirm_cpu_cleanup(
        &mut self,
        command: &str,
        r: &worker::CpuCleanupReceipt,
    ) -> Result<(), ExecutionJournalError> {
        let bytes = r.encode_to_vec();
        let prior: Option<Vec<u8>> = self.connection.query_row(
            "SELECT receipt FROM cpu_cleanup_facts WHERE command_id=?",
            [command],
            |row| row.get(0),
        )?;
        if prior.as_ref().is_some_and(|p| p != &bytes) {
            return Err(ExecutionJournalError::Invalid);
        }
        if self.connection.execute(
            "UPDATE cpu_cleanup_facts SET receipt=?,request_digest=? WHERE command_id=?",
            params![bytes, r.request_digest, command],
        )? != 1
        {
            return Err(ExecutionJournalError::Stale);
        }
        Ok(())
    }
}
