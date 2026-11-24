use super::*;
const MARKER: &str = ".orange-artifact-root";
pub(super) struct RootMarker {
    id: String,
    version: String,
}
impl RootMarker {
    pub(super) async fn bind(
        pool: &PgPool,
        op: &Operator,
        root: &str,
        writable: bool,
    ) -> Result<Self, ArtifactError> {
        if writable {
            sqlx::query("INSERT INTO artifact_s3_bindings(root_id,marker_id) VALUES($1,$2::text::uuid) ON CONFLICT DO NOTHING")
                .bind(root).bind(uuid::Uuid::new_v4().to_string()).execute(pool).await?;
        }
        let (id, pinned): (String, Option<String>) = sqlx::query_as(
            "SELECT marker_id::text,provider_version FROM artifact_s3_bindings WHERE root_id=$1",
        )
        .bind(root)
        .fetch_optional(pool)
        .await?
        .ok_or(ArtifactError::StoragePolicy)?;
        let version = match pinned {
            Some(v) => v,
            None if writable => {
                let meta = match op.stat(MARKER).await {
                    Ok(existing) => existing,
                    Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                        op.write_with(MARKER, id.clone())
                            .if_not_exists(true)
                            .await?
                    }
                    Err(e) => return Err(e.into()),
                };
                meta.version()
                    .filter(|s| safe_version(s))
                    .ok_or(ArtifactError::StoragePolicy)?
                    .to_owned()
            }
            None => return Err(ArtifactError::StoragePolicy),
        };
        let marker = Self { id, version };
        marker.verify(op).await?;
        if writable {
            sqlx::query("UPDATE artifact_s3_bindings SET provider_version=$2 WHERE root_id=$1 AND provider_version IS NULL")
                .bind(root).bind(&marker.version).execute(pool).await?;
        }
        Ok(marker)
    }
    pub(super) async fn verify(&self, op: &Operator) -> Result<(), ArtifactError> {
        let meta = op.stat(MARKER).await?;
        if meta.version() != Some(self.version.as_str()) || meta.content_length() != 36 {
            return Err(ArtifactError::StoragePolicy);
        }
        let data = op
            .read_with(MARKER)
            .version(&self.version)
            .range(0..36)
            .await?;
        if data.to_bytes().as_ref() != self.id.as_bytes() {
            return Err(ArtifactError::StoragePolicy);
        }
        Ok(())
    }
}
