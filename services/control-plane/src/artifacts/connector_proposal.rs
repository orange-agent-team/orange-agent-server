//! Only a settled first-turn response committed to this Run may supply operation arguments.
//! Public artifact downloads still use the final-output visibility boundary.
use super::*;
use sha2::Digest;

impl ArtifactStore {
    pub(crate) async fn committed_connector_response(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        run: &str,
        job: &str,
        commit: &str,
    ) -> Result<orange_protocol::model::ResponseCandidate, ArtifactError> {
        let snapshot = || async {
            let mut tx = self.inner.pool.begin().await?;
            authority::scope(&mut tx, actor, owner, false).await?;
            let row=sqlx::query("SELECT dst.root_id,dst.size_bytes,dst.sha256,x.published_key FROM cloud_connector_model_commits m
                JOIN cloud_runs r USING(run_id) JOIN cloud_model_plans plan USING(run_id)
                JOIN cloud_job_turns jt ON jt.job_id=m.job_id AND jt.run_id=r.run_id
                JOIN cloud_model_turns t ON t.turn_id=jt.turn_id AND t.run_id=r.run_id AND t.ordinal=1
                JOIN jobs j ON j.job_id=m.job_id AND j.owner_id=r.owner_id AND j.root_budget_id=r.root_budget_id
                JOIN job_response_commits c ON c.job_id=j.job_id AND c.commit_id=m.commit_id AND c.artifact_id=m.artifact_id AND c.version_id=m.version_id AND c.response_digest=m.response_digest
                JOIN job_settlements s ON s.job_id=j.job_id
                JOIN artifacts src ON src.artifact_id=r.input_artifact_id AND src.version_id=r.input_version_id AND src.owner_id=r.owner_id
                JOIN artifacts dst ON dst.artifact_id=c.artifact_id AND dst.version_id=c.version_id AND dst.owner_id=r.owner_id
                JOIN artifact_transfers x ON x.transfer_id=dst.published_transfer_id AND x.artifact_id=dst.artifact_id
                WHERE r.run_id=$1 AND r.user_id=$2 AND r.owner_id=$3 AND m.job_id=$4 AND m.commit_id=$5 AND r.status<>'cancelled'
                  AND src.state='available' AND orange_artifact_content_live(src.artifact_id) AND dst.state='available' AND orange_artifact_content_live(dst.artifact_id) AND x.state='attached' AND NOT x.published_deleted
                FOR SHARE OF r,src,dst,x").bind(run).bind(&actor.user).bind(owner).bind(job).bind(commit).fetch_optional(&mut *tx).await?.ok_or(ArtifactError::NotReady)?;
            let root: String = row.try_get("root_id")?;
            let size: i64 = row.try_get("size_bytes")?;
            if root != self.inner.objects.root_id || !(1..=1048576).contains(&size) {
                return Err(ArtifactError::Integrity);
            }
            let result = (
                row.try_get::<String, _>("published_key")?,
                size as u64,
                row.try_get::<String, _>("sha256")?,
            );
            tx.commit().await?;
            Ok(result)
        };
        let stamp = snapshot().await?;
        let buffer = self.inner.objects.read(&stamp.0, stamp.1).await?.to_bytes();
        if hex::encode(sha2::Sha256::digest(&buffer)) != stamp.2 || snapshot().await? != stamp {
            return Err(ArtifactError::Integrity);
        }
        let doc = orange_protocol::canonical::parse_tool_arguments(
            std::str::from_utf8(&buffer).map_err(|_| ArtifactError::Integrity)?,
        )
        .map_err(|_| ArtifactError::Integrity)?;
        serde_json::from_value(doc).map_err(|_| ArtifactError::Integrity)
    }
}
