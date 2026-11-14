//! Content-free discovery for trusted recovery UIs. This neither opens files
//! nor restores read/edit/recovery authority, and never retries an operation.
use super::*;

impl Journal {
    /// Descending immutable operation IDs; use the last returned ID as the next
    /// `before` cursor. A revoked Run may still have unresolved effects, so its
    /// authenticated owner can discover IDs. Inspection and resolution require
    /// separate current filesystem capabilities. No paths, arguments, hashes or
    /// retained contents are returned by this metadata-only index.
    pub fn agent_unresolved_patch_ids(
        &self,
        owner: &str,
        run: &str,
        before: Option<&str>,
        limit: u16,
    ) -> Result<Vec<String>, EngineError> {
        if !orange_protocol::valid_id(owner)
            || !orange_protocol::valid_id(run)
            || before.is_some_and(|value| !orange_protocol::valid_id(value))
            || !(1..=100).contains(&limit)
        {
            return Err(EngineError::Invalid);
        }
        let owned: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs JOIN runtime_runs USING(run_id) WHERE run_id=? AND owner=?)",
            params![run,owner], |r|r.get(0),
        )?;
        if !owned {
            return Err(EngineError::Forbidden);
        }
        let mut statement = self.connection.prepare(
            "SELECT o.operation_id FROM operations o JOIN editing_publications p USING(operation_id)
             WHERE o.owner=?1 AND o.run_id=?2 AND o.tool_version='apply_patch.v1' AND o.state='unknown'
             AND (?3 IS NULL OR o.operation_id<?3)
             AND NOT EXISTS(SELECT 1 FROM patch_recovery_resolutions r WHERE r.operation_id=o.operation_id)
             ORDER BY o.operation_id DESC LIMIT ?4",
        )?;
        statement
            .query_map(params![owner, run, before, i64::from(limit)], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(EngineError::from)
    }
}
