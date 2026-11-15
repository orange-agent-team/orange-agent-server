//! Explicitly negotiated v2 entry/list execution; no model-granted authority.
use super::*;
use crate::memory::{MemorySummary, MemoryValue, tools::MemoryReadV2Args};
use serde_json::{Value, json};

enum ReadResult {
    Entry(Box<VerifiedMemoryRead>),
    List(Box<VerifiedMemoryPage>),
}
impl ReadResult {
    fn project(&self, j: &Journal) -> Result<Value, EngineError> {
        Ok(match self {
            Self::Entry(v) => {
                json!({"kind":"memory_read","data":{"mode":"entry","value":v.view(j)?}})
            }
            Self::List(v) => json!({"kind":"memory_read","data":{"mode":"list","page":v.view(j)?}}),
        })
    }
}
pub(super) fn expected_sources(
    args: &MemoryReadV2Args,
    cached: &Value,
) -> Result<Vec<(String, String, i64)>, EngineError> {
    let mut rows = vec![];
    let mut add = |id: &str, version: &str| -> Result<(), EngineError> {
        let version = version
            .parse::<i64>()
            .map_err(|_| EngineError::RecoveryRequired)?;
        if !valid_id(id) || version <= 0 {
            return Err(EngineError::RecoveryRequired);
        }
        rows.push((args.scope().to_owned(), id.to_owned(), version));
        Ok(())
    };
    match args {
        MemoryReadV2Args::Entry { entry_id, .. } => {
            let v: MemoryValue = serde_json::from_value(cached["data"]["value"].clone())
                .map_err(|_| EngineError::RecoveryRequired)?;
            if cached["data"]["mode"] != "entry"
                || v.scope_id != args.scope()
                || v.entry_id != *entry_id
                || !v.enabled
            {
                return Err(EngineError::RecoveryRequired);
            }
            add(&v.entry_id, &v.version)?;
        }
        MemoryReadV2Args::List { limit, .. } => {
            let page: ManagedMemoryPage = serde_json::from_value(cached["data"]["page"].clone())
                .map_err(|_| EngineError::RecoveryRequired)?;
            if cached["data"]["mode"] != "list"
                || page.scope_id != args.scope()
                || page.entries.len() > usize::from(limit.unwrap_or(20))
                || page.entries.iter().any(|v| !v.enabled)
            {
                return Err(EngineError::RecoveryRequired);
            }
            for v in &page.entries {
                add(&v.entry_id, &v.version)?;
            }
        }
    }
    rows.sort();
    if rows.windows(2).any(|v| v[0].1 == v[1].1) {
        return Err(EngineError::RecoveryRequired);
    }
    Ok(rows)
}
pub(super) fn check_current_projection(
    args: &MemoryReadV2Args,
    cached: &Value,
    value: &MemoryValue,
) -> Result<(), EngineError> {
    match args {
        MemoryReadV2Args::Entry { .. } => {
            if serde_json::to_value(value).map_err(|_| EngineError::Invalid)?
                != cached["data"]["value"]
            {
                return Err(EngineError::RecoveryRequired);
            }
        }
        MemoryReadV2Args::List { .. } => {
            let page: ManagedMemoryPage = serde_json::from_value(cached["data"]["page"].clone())
                .map_err(|_| EngineError::RecoveryRequired)?;
            let expected = MemorySummary {
                entry_id: value.entry_id.clone(),
                version: value.version.clone(),
                title: value.title.clone(),
                enabled: value.enabled,
                origin: value.origin,
                content_bytes: value.total_bytes.clone(),
            };
            if page
                .entries
                .iter()
                .filter(|v| v.entry_id == value.entry_id)
                .collect::<Vec<_>>()
                != vec![&expected]
            {
                return Err(EngineError::RecoveryRequired);
            }
        }
    }
    Ok(())
}
impl Journal {
    pub(super) async fn advance_memory_discovery(
        &mut self,
        stored: &StoredRun,
        op: &StoredOperation,
        authority: AgentAuthority<'_>,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), EngineError> {
        let AgentRequest::Memory(MemoryRequest::MemoryReadV2(args)) = &op.request else {
            return Err(EngineError::Invalid);
        };
        if stored.memory_read_version != 2 {
            return Err(EngineError::RecoveryRequired);
        }
        let c = &stored.config;
        let grant = self.memory_grant_for(stored, op, authority.memories)?;
        let workspaces = std::iter::once(authority.read.clone())
            .chain(authority.source_workspaces.iter().cloned())
            .collect::<Vec<_>>();
        let memories = authority
            .memories
            .iter()
            .chain(authority.source_memories.iter())
            .filter(|v| v.scope_id() != grant.scope_id())
            .cloned()
            .collect::<Vec<_>>();
        let management = MemoryManagementAuthority::new(grant)
            .with_source_workspaces(&workspaces)
            .with_source_memories(&memories);
        let result = match args {
            MemoryReadV2Args::Entry {
                entry_id,
                expected_version,
                start_byte,
                max_bytes,
                ..
            } => self
                .read_memory_verified(
                    management,
                    MemoryReadSelection {
                        entry_id: entry_id.clone(),
                        expected_version: expected_version.clone(),
                        start_byte: start_byte.unwrap_or(0),
                        max_bytes: max_bytes.unwrap_or(4096),
                        include_disabled: false,
                    },
                )
                .await
                .map(|v| ReadResult::Entry(Box::new(v))),
            MemoryReadV2Args::List { cursor, limit, .. } => self
                .list_memory_verified(management, cursor.as_deref(), limit.unwrap_or(20), false)
                .await
                .map(|v| ReadResult::List(Box::new(v))),
        };
        self.memory_grant_for(stored, op, authority.memories)?;
        check_grant(c, authority.read)?;
        if *cancel.borrow() || cancel.has_changed().is_err() || now()? >= c.deadline_unix {
            return Err(EngineError::Forbidden);
        }
        let (value, sources, failed) = match &result {
            Ok(receipt) => {
                let value = receipt.project(self)?;
                let refs = expected_sources(args, &value)?;
                (value, refs, false)
            }
            Err(EngineError::Storage(_) | EngineError::Journal(_)) => {
                return Err(result.err().ok_or(EngineError::Invalid)?);
            }
            Err(_) => (json!({"error":"memory_unavailable"}), vec![], true),
        };
        self.validate_tool_output(c, op, &value, crate::catalog::OutputChannel::Durable)?;
        self.validate_tool_output(c, op, &value, crate::catalog::OutputChannel::Model)?;
        self.claim(&op.operation(c), now()?)?;
        self.connection.execute(
            "UPDATE runtime_runs SET steps=steps+1 WHERE run_id=?",
            [&c.run_id],
        )?;
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        for (scope, id, version) in sources {
            tx.execute(
                "INSERT INTO runtime_memory_sources VALUES(?,?,?,?)",
                params![op.id, scope, id, version],
            )?;
        }
        if failed {
            if tx.execute("UPDATE operations SET state='failed',error_code='memory_unavailable',model_result=? WHERE operation_id=? AND state='dispatching'",params![encode(&value)?,op.id])?!=1{return Err(EngineError::RecoveryRequired);}
        } else if tx.execute("UPDATE operations SET state='succeeded',result_digest=?,result_json=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![digest("memory-tool-result.v1",&value)?,encode(&value)?,encode(&value)?,op.id])?!=1 {return Err(EngineError::RecoveryRequired);}
        tx.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,?)",
            params![
                c.run_id,
                op.id,
                if failed {
                    "operation.failed"
                } else {
                    "operation.succeeded"
                }
            ],
        )?;
        if let Ok(receipt) = &result
            && receipt.project(self)? != value
        {
            return Err(EngineError::RecoveryRequired);
        }
        self.memory_grant_for(stored, op, authority.memories)?;
        check_grant(c, authority.read)?;
        let time = now()?;
        let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM approvals a JOIN operations o USING(operation_id) JOIN runs r USING(run_id) WHERE o.operation_id=? AND r.active=1 AND a.argument_digest=o.argument_digest AND a.environment=o.environment AND a.policy=o.policy AND a.expires_at>?)",params![op.id,time],|r|r.get(0))?;
        if !live || time >= c.deadline_unix || *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(EngineError::Forbidden);
        }
        tx.commit()?;
        self.apply_tool_context(stored, &self.operation_record(c, &op.id)?)
    }
}
