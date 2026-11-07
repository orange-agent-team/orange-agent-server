use super::*;
impl ExecutionJournal {
    pub(crate) fn bind_cpu_root(
        &mut self,
        root: &std::path::Path,
    ) -> Result<(), ExecutionJournalError> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(root).map_err(BrokerError::from)?;
        if !root.is_absolute() || !m.is_dir() || m.file_type().is_symlink() || m.mode() & 0o077 != 0
        {
            return Err(ExecutionJournalError::Invalid);
        }
        let path = root
            .canonicalize()
            .map_err(BrokerError::from)?
            .to_str()
            .ok_or(ExecutionJournalError::Invalid)?
            .to_owned();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, String, String)> = tx
            .query_row(
                "SELECT root_path,device,inode FROM cpu_runtime_binding WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let current = (path, m.dev().to_string(), m.ino().to_string());
        if let Some(previous) = previous {
            if previous != current {
                return Err(ExecutionJournalError::Stale);
            }
        } else {
            tx.execute(
                "INSERT INTO cpu_runtime_binding(singleton,root_path,device,inode) VALUES(1,?,?,?)",
                params![current.0, current.1, current.2],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn begin_cpu(
        &mut self,
        identity: &ControlIdentity,
        command: &str,
        instance: &str,
        request: &str,
        profile: &str,
    ) -> Result<(), ExecutionJournalError> {
        self.current(identity)?;
        let record = self.record(command)?.ok_or(ExecutionJournalError::Stale)?;
        if record.state != "running"
            || record.assignment.session_id != identity.session_id
            || record.assignment.request_projection_digest != request
            || record.assignment.execution_profile_digest != profile
            || !valid_id(instance)
        {
            return Err(ExecutionJournalError::Stale);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO cpu_runs(command_id,instance_id,request_digest,profile_digest,phase) VALUES(?,?,?,?,'starting')",params![command,instance,request,profile])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn cpu_phase(
        &mut self,
        command: &str,
        phase: &str,
    ) -> Result<(), ExecutionJournalError> {
        if phase != "generating" {
            return Err(ExecutionJournalError::Invalid);
        }
        if self.connection.execute(
            "UPDATE cpu_runs SET phase='generating' WHERE command_id=? AND phase='starting'",
            [command],
        )? != 1
        {
            return Err(ExecutionJournalError::Stale);
        }
        Ok(())
    }
    pub(crate) fn finish_cpu(
        &mut self,
        command: &str,
        phase: &str,
        response: Option<&str>,
        cleanup: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), ExecutionJournalError> {
        if !matches!(
            phase,
            "computed_unsubmitted" | "cancelled" | "failed" | "authority_lost" | "cleanup_unknown"
        ) || response.is_some_and(|v| !orange_protocol::valid_digest(v))
            || cleanup.is_some_and(|v| !orange_protocol::valid_digest(v))
        {
            return Err(ExecutionJournalError::Invalid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("UPDATE cpu_runs SET phase=?,response_digest=?,cleanup_digest=?,cleanup_confirmed=?,error_code=? WHERE command_id=? AND phase IN('starting','generating')",params![phase,response,cleanup,cleanup.is_some(),error,command])?!=1{return Err(ExecutionJournalError::Stale);}
        tx.commit()?;
        Ok(())
    }
}
