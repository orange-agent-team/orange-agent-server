//! Owner-authorized content measurements. This read has no job, pricing or ledger effects.
use super::*;
use orange_model_runtime::image_artifact::{ImageContent, MAX_PNG, inspect};
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;

#[derive(Serialize)]
pub(super) struct InspectedImage {
    artifact_id: String,
    version_id: String,
    content: ImageContent,
}

impl ArtifactStore {
    pub(super) async fn inspect_image_output(
        &self,
        actor: &ArtifactActor,
        owner: &str,
        upload: &str,
        version: &str,
        permit: OwnedSemaphorePermit,
    ) -> Result<InspectedImage, ArtifactError> {
        if !valid_id(upload) || !valid_id(version) {
            return Err(ArtifactError::Invalid);
        }
        let mut tx = self.inner.pool.begin().await?;
        authority::scope(&mut tx, actor, owner, false).await?;
        let row = self.row_in(&mut tx, owner, upload).await?;
        authority::resource(&mut tx, actor, owner, &row, false, false).await?;
        let source = view(&row)?;
        authority::finish(tx, actor).await?;
        if source.version_id != version {
            return Err(ArtifactError::Conflict);
        }
        if source.state != "available" {
            return Err(ArtifactError::NotReady);
        }
        if source.media_type != "image/png"
            || source
                .size_bytes
                .parse::<usize>()
                .map_err(|_| ArtifactError::Integrity)?
                > MAX_PNG
        {
            return Err(ArtifactError::Invalid);
        }
        let (source, bytes, stamp) = self.read(actor, owner, upload, version).await?;
        // Keep decode admission until the blocking task exits, including when the browser disconnects.
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            inspect(&bytes.to_bytes())
        })
        .await;
        self.finish_image_inspection(
            actor,
            source,
            &stamp,
            result
                .map_err(|_| ArtifactError::Unavailable)
                .and_then(|r| r.map_err(|_| ArtifactError::Invalid)),
        )
        .await
    }

    pub(super) async fn finish_image_inspection(
        &self,
        actor: &ArtifactActor,
        source: ArtifactView,
        stamp: &ReadStamp,
        decoded: Result<ImageContent, ArtifactError>,
    ) -> Result<InspectedImage, ArtifactError> {
        // Recheck the content ACL and retirement stamp before revealing either pixels or decoder errors.
        self.check_read_scope(actor, &source, stamp, false).await?;
        let content = decoded?;
        if content.sha256 != source.sha256 || content.bytes.to_string() != source.size_bytes {
            return Err(ArtifactError::Integrity);
        }
        Ok(InspectedImage {
            artifact_id: source.artifact_id,
            version_id: source.version_id,
            content,
        })
    }
}
