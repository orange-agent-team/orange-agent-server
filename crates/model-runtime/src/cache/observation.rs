//! Read-only diagnostics for one content-addressed GGUF in a private cache.
use super::*;
use serde::Serialize;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    Absent,
    Staged,
    Verified,
    Invalid,
}
#[derive(Debug, Serialize)]
pub struct CacheObservation {
    pub sha256: String,
    pub expected_bytes: u64,
    pub state: CacheState,
    pub download_staging_bytes: Option<u64>,
    pub import_staging_bytes: Option<u64>,
    /// Some only when a published file was observed. Reports cooperating filesystem locks.
    pub read_lease_observed: Option<bool>,
}
fn optional_file(path: &Path) -> Result<Option<File>, CacheError> {
    match private_file(path, false) {
        Ok(file) => Ok(Some(file)),
        Err(CacheError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
impl ModelCache {
    pub async fn observe(root: &Path, spec: &ArtifactSpec) -> Result<CacheObservation, CacheError> {
        let mut result = CacheObservation {
            sha256: spec.digest.clone(),
            expected_bytes: spec.bytes,
            state: CacheState::Absent,
            download_staging_bytes: None,
            import_staging_bytes: None,
            read_lease_observed: None,
        };
        match fs::symlink_metadata(root) {
            Ok(meta) => validate_root(&meta)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(result),
            Err(e) => return Err(e.into()),
        }
        let named = |suffix: &str| root.join(format!("{}.{suffix}", spec.digest));
        let bytes = |suffix: &str| -> Result<Option<u64>, CacheError> {
            Ok(optional_file(&named(suffix))?
                .map(|f| f.metadata().map(|m| m.len()))
                .transpose()?)
        };
        result.download_staging_bytes = bytes("part")?;
        result.import_staging_bytes = bytes("import")?;
        let ready = named("gguf");
        if let Some(probe) = optional_file(&ready)? {
            result.read_lease_observed = Some(match probe.try_lock() {
                Ok(()) => {
                    probe.unlock()?;
                    false
                }
                Err(fs::TryLockError::WouldBlock) => true,
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            });
            result.state = match verify(&ready, spec).await {
                Ok(file) if same_snapshot(&probe, &file.0)? => CacheState::Verified,
                Ok(_)
                | Err(CacheError::Integrity | CacheError::Incomplete | CacheError::TooLarge) => {
                    CacheState::Invalid
                }
                Err(e) => return Err(e),
            };
        } else if [result.download_staging_bytes, result.import_staging_bytes]
            .into_iter()
            .flatten()
            .any(|bytes| bytes > spec.bytes)
        {
            result.state = CacheState::Invalid;
        } else if result.download_staging_bytes.is_some() || result.import_staging_bytes.is_some() {
            result.state = CacheState::Staged;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn observe_distinguishes_partial_invalid_and_leased_publications() {
        let tmp = tempfile::tempdir().expect("test directory");
        let root = tmp.path().join("cache");
        let contents = b"GGUF sample";
        let digest = hex::encode(Sha256::digest(contents));
        let spec =
            ArtifactSpec::local_only(&digest, contents.len() as u64, b"GGUF").expect("descriptor");
        let absent = ModelCache::observe(&root, &spec).await.expect("absent");
        assert_eq!(absent.state, CacheState::Absent);
        assert!(!root.exists(), "observation must not create a cache");
        let cache = ModelCache::new(&root, 1000).expect("private cache");
        fs::write(root.join(format!("{digest}.part")), &contents[..4]).expect("partial");
        let staged = ModelCache::observe(&root, &spec).await.expect("staging");
        assert_eq!(staged.state, CacheState::Staged);
        assert_eq!(staged.download_staging_bytes, Some(4));
        let path = root.join(format!("{digest}.gguf"));
        fs::write(&path, b"GGUF altered").expect("unverified publication");
        assert_eq!(
            ModelCache::observe(&root, &spec)
                .await
                .expect("invalid length")
                .state,
            CacheState::Invalid
        );
        fs::write(&path, b"GGUF false!").expect("same-size mutation");
        assert_eq!(
            ModelCache::observe(&root, &spec)
                .await
                .expect("wrong digest")
                .state,
            CacheState::Invalid
        );
        fs::write(&path, contents).expect("valid bytes");
        let first = cache.ensure(&spec).await.expect("first reader");
        let second = cache.ensure(&spec).await.expect("second reader");
        drop(first);
        let pinned = ModelCache::observe(&root, &spec)
            .await
            .expect("another reader still holds verified file");
        assert_eq!(pinned.state, CacheState::Verified);
        assert_eq!(pinned.read_lease_observed, Some(true));
        let incidental = second
            ._read_lease
            .0
            .try_clone()
            .expect("incidental duplicate file descriptor");
        drop(second);
        assert_eq!(
            ModelCache::observe(&root, &spec)
                .await
                .expect("no active lease")
                .read_lease_observed,
            Some(false)
        );
        drop(incidental);
        // Different inodes can have exactly the same valid bytes. The returned receipt must identify the checked inode.
        let verified = verify(&path, &spec).await.expect("measure");
        fs::rename(&path, root.join("original")).expect("retire original name");
        fs::write(&path, contents).expect("new inode");
        assert!(matches!(
            artifact(&path, &spec, true, verified),
            Err(CacheError::Integrity)
        ));
    }
}
