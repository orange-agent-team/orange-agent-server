//! Independent read selection for retained-file management. It cannot become
//! an Agent workspace, an editing grant, or a recovery write grant.
use super::*;
mod export;
pub use export::{
    ExportFileObservation, ExportInspectionGrant, ExportInspectionReceipt, ExportInspectionView,
    ExportQuiescence, PreparedRetentionExport, RetentionExportCapture, RetentionExportChoice,
    RetentionExportDestination, RetentionExportEntrySummary, RetentionExportOutcome,
    RetentionExportPreview, RetentionExportReceipt, RetentionExportReview, verify_retention_export,
};
use std::path::Path;

pub struct PatchRetentionGrant {
    read: WorkspaceGrant,
    run: String,
    operation: String,
    epoch: i64,
}
impl PatchRetentionGrant {
    pub fn request_revoke(&self) {
        self.read.request_revoke();
    }
    pub async fn wait_drained(&self) {
        self.read.wait_drained().await;
    }
}
impl Journal {
    /// Trusted host only, following a fresh native directory selection for
    /// retention management. File access is scoped to this owned operation.
    pub fn authorize_patch_retention(
        &self,
        owner: &str,
        run: &str,
        operation: &str,
        root: &Path,
    ) -> Result<PatchRetentionGrant, PatchError> {
        if ![owner, run, operation].iter().all(|id| valid_id(id)) {
            return Err(PatchError::Invalid);
        }
        let row: Option<(String, String)> = self.connection.query_row(
            "SELECT o.policy,p.resource_identity FROM operations o
             JOIN editing_publications p USING(operation_id)
             JOIN runtime_runs r ON r.run_id=o.run_id
             WHERE o.owner=? AND o.run_id=? AND o.operation_id=? AND o.tool_version='apply_patch.v1'",
            params![owner, run, operation], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(JournalError::from)?;
        let (policy, resource) = row.ok_or(WorkspaceError::Forbidden)?;
        self.retention_terminal(owner, operation)?;
        let mut private = self.extra_private_roots.clone();
        private.push(self.private_root.clone());
        let read = WorkspaceGrant::open(
            owner,
            &uuid::Uuid::new_v4().to_string(),
            &policy,
            root,
            &private,
        )?;
        if !read.has_persistent_identity() || read.resource_identity() != resource {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let epoch = self.retention_scope(owner, &read)?;
        let grant = PatchRetentionGrant {
            read,
            run: run.into(),
            operation: operation.into(),
            epoch,
        };
        self.validate_retention_selection(&grant)?;
        self.connection.execute(
            "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'patch.retention_selection_consent')",
            params![run, operation],
        ).map_err(JournalError::from)?;
        Ok(grant)
    }

    fn validate_retention_selection(&self, grant: &PatchRetentionGrant) -> Result<(), PatchError> {
        if self.retention_scope(grant.read.owner(), &grant.read)? != grant.epoch {
            return Err(WorkspaceError::Revoked.into());
        }
        let owned: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations o JOIN editing_publications p USING(operation_id)
             JOIN runtime_runs r ON r.run_id=o.run_id
             WHERE o.owner=? AND o.run_id=? AND o.operation_id=? AND o.tool_version='apply_patch.v1'
             AND o.policy=? AND p.resource_identity=?)",
            params![grant.read.owner(), grant.run, grant.operation, grant.read.policy(), grant.read.resource_identity()],
            |row| row.get(0),
        ).map_err(JournalError::from)?;
        if !owned {
            return Err(WorkspaceError::Forbidden.into());
        }
        self.retention_terminal(grant.read.owner(), &grant.operation)?;
        Ok(())
    }

    pub async fn inspect_selected_retention(
        &self,
        grant: &PatchRetentionGrant,
    ) -> Result<PatchRecoveryReceipt, PatchError> {
        self.validate_retention_selection(grant)?;
        let receipt = self
            .inspect_patch_recovery(grant.read.owner(), &grant.operation, &grant.read)
            .await?;
        self.validate_retention_selection(grant)?;
        Ok(receipt)
    }

    pub fn selected_retention_usage(
        &self,
        grant: &PatchRetentionGrant,
    ) -> Result<PatchRetentionUsage, PatchError> {
        self.validate_retention_selection(grant)?;
        self.patch_retention_usage(grant.read.owner(), &grant.read)
    }

    pub async fn prepare_selected_retention(
        &mut self,
        grant: &PatchRetentionGrant,
        request_key: &str,
    ) -> Result<PatchRetentionReview, PatchError> {
        self.validate_retention_selection(grant)?;
        let review = self
            .prepare_patch_retention(
                grant.read.owner(),
                &grant.operation,
                &grant.read,
                request_key,
            )
            .await?;
        self.validate_retention_selection(grant)?;
        Ok(review)
    }

    /// Explicit owner approval of the exact preview; selection alone never
    /// changes accounting. The underlying transaction is idempotent by review.
    pub async fn approve_selected_retention(
        &mut self,
        grant: &PatchRetentionGrant,
        review: &str,
        digest: &str,
    ) -> Result<PatchRetentionArchive, PatchError> {
        self.validate_retention_selection(grant)?;
        let record = self.retention_record(grant.read.owner(), &grant.read, review, digest)?;
        if record.plan.operation_id != grant.operation {
            return Err(WorkspaceError::Forbidden.into());
        }
        let archive = self
            .approve_patch_retention(
                grant.read.owner(),
                &grant.read,
                review,
                digest,
                grant.read.owner(),
            )
            .await?;
        self.validate_retention_selection(grant)?;
        Ok(archive)
    }

    pub async fn inspect_selected_archive(
        &self,
        grant: &PatchRetentionGrant,
    ) -> Result<PatchArchiveInspection, PatchError> {
        self.validate_retention_selection(grant)?;
        let receipt = self
            .inspect_patch_archive(grant.read.owner(), &grant.operation, &grant.read)
            .await?;
        self.validate_retention_selection(grant)?;
        Ok(receipt)
    }
}
