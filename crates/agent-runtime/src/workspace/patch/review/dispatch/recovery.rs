//! Journal evidence only. Last durable intent is not proof of current files;
//! reading this view never grants access, inspects names or resumes dispatch.
use super::*;
#[cfg(target_vendor = "apple")]
pub mod inspection;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicationStage {
    StageIntent,
    Staged,
    CaptureIntent,
    Captured,
    PublishIntent,
    Published,
    Confirmed,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationRecoveryView {
    pub plan: PublicationPlan,
    pub execution_state: String,
    pub last_durable_stage: Option<PublicationStage>,
    pub committed_receipt: Option<PublishedPatch>,
}
impl Journal {
    pub fn patch_recovery_view(
        &self,
        owner: &str,
        operation: &str,
    ) -> Result<Option<PublicationRecoveryView>, PatchError> {
        let Some(plan) = self.patch_publication_plan(owner, operation)? else {
            return Ok(None);
        };
        let (state,body,digest): (String,Option<String>,Option<String>) = self.connection.query_row("SELECT state,result_json,result_digest FROM operations WHERE operation_id=? AND owner=?",params![operation,owner],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(JournalError::from)?;
        if !["dispatching", "unknown", "succeeded", "failed"].contains(&state.as_str()) {
            return Err(JournalError::Conflict.into());
        }
        let mut statement=self.connection.prepare("SELECT step,body FROM editing_publication_steps WHERE operation_id=? ORDER BY step LIMIT 8").map_err(JournalError::from)?;
        let rows = statement
            .query_map([operation], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(JournalError::from)?;
        let mut count = 0;
        let mut last = None;
        let mut confirmed = None;
        for row in rows {
            let (step, stage_body) = row.map_err(JournalError::from)?;
            count += 1;
            if step != count || step > 7 || stage_body.len() > 16 * 1024 {
                return Err(JournalError::Conflict.into());
            }
            let (stage, kind) = match step {
                1 => (PublicationStage::StageIntent, "stage_intent"),
                2 => (PublicationStage::Staged, "staged"),
                3 => (PublicationStage::CaptureIntent, "capture_intent"),
                4 => (PublicationStage::Captured, "captured"),
                5 => (PublicationStage::PublishIntent, "publish_intent"),
                6 => (PublicationStage::Published, "published"),
                7 => (PublicationStage::Confirmed, ""),
                _ => return Err(JournalError::Conflict.into()),
            };
            if step == 7 {
                confirmed = Some(
                    serde_json::from_str::<PublishedPatch>(&stage_body)
                        .map_err(|_| PatchError::Invalid)?,
                );
            } else {
                let data: serde_json::Value =
                    serde_json::from_str(&stage_body).map_err(|_| PatchError::Invalid)?;
                if data.get("kind").and_then(|v| v.as_str()) != Some(kind) {
                    return Err(JournalError::Conflict.into());
                }
            }
            last = Some(stage);
        }
        let receipt = if state == "succeeded" {
            let (Some(body), Some(digest), Some(confirmed)) = (body, digest, confirmed) else {
                return Err(JournalError::Conflict.into());
            };
            if body.len() > 16 * 1024 {
                return Err(PatchError::Limit);
            }
            let receipt: PublishedPatch =
                serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            if receipt != confirmed
                || receipt.path != plan.path
                || receipt.original_sha256 != plan.original_sha256
                || receipt.result_sha256 != plan.result_sha256
                || receipt.retained_original != plan.backup_name
                || receipt
                    .bytes
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n <= MAX_TEXT && n.to_string() == receipt.bytes)
                    .is_none()
                || request_digest("published-patch.v1", &receipt)
                    .map_err(|_| PatchError::Invalid)?
                    != digest
            {
                return Err(JournalError::Conflict.into());
            }
            Some(receipt)
        } else {
            if body.is_some() || digest.is_some() || confirmed.is_some() {
                return Err(JournalError::Conflict.into());
            }
            None
        };
        Ok(Some(PublicationRecoveryView {
            plan,
            execution_state: state,
            last_durable_stage: last,
            committed_receipt: receipt,
        }))
    }
}
