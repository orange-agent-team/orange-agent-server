//! Cooperating requests pin download staging independently of verified model readers.
use super::*;
use tokio::sync::watch;

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StagingCleanup {
    Released {
        download_bytes: u64,
        import_bytes: u64,
    },
    RetainedForSubscribers,
    WriterBusy,
}

impl ModelCache {
    /// Read the verified result or wait for a competing writer, while this caller retains interest.
    /// When its last sender disappears, the request also relinquishes its subscription.
    pub async fn ensure_with_cancel(
        &self,
        spec: &ArtifactSpec,
        cancel: watch::Receiver<bool>,
    ) -> Result<VerifiedArtifact, CacheError> {
        if spec.url.is_none() {
            let mut stop = cancel;
            return tokio::select! {biased;_=stop.wait_for(|flag|*flag)=>Err(CacheError::Cancelled),result=self.ensure(spec)=>result};
        }
        self.subscribed(spec, &http_transport(spec)?, cancel).await
    }

    pub(super) async fn subscribed<T: Transport>(
        &self,
        spec: &ArtifactSpec,
        transport: &T,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<VerifiedArtifact, CacheError> {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(CacheError::Cancelled);
        }
        let name = self.root.join(format!("{}.subscriptions", spec.digest));
        let interest = private_file(&name, true)?;
        let until = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            match interest.try_lock_shared() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < until => {
                    tokio::select! {biased;_=cancel.wait_for(|flag|*flag)=>return Err(CacheError::Cancelled),_=tokio::time::sleep(Duration::from_millis(50))=>{}}
                }
                Err(fs::TryLockError::WouldBlock) => return Err(CacheError::LockUnavailable),
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        let interest = ReadLease(interest);
        let acquire = async {
            loop {
                match self.ensure_with(spec, transport).await {
                    Err(CacheError::LockUnavailable) if tokio::time::Instant::now() < until => {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                    result => return result,
                }
            }
        };
        let result = tokio::select! {
            biased;
            _=cancel.wait_for(|flag|*flag)=>Err(CacheError::Cancelled),
            result=acquire=>result,
        };
        drop(interest);
        if matches!(result, Err(CacheError::Cancelled)) {
            // A blocking disk write may briefly outlive cancellation of its async caller.
            let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
            loop {
                match self.cleanup(spec, false)? {
                    StagingCleanup::WriterBusy if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(25)).await
                    }
                    StagingCleanup::WriterBusy => return Err(CacheError::CleanupPending),
                    _ => break,
                }
            }
        }
        result
    }

    /// Explicit reclamation of this descriptor's transient files only. Retriable after WriterBusy.
    pub fn discard_staging(&self, spec: &ArtifactSpec) -> Result<StagingCleanup, CacheError> {
        self.cleanup(spec, true)
    }

    fn cleanup(
        &self,
        spec: &ArtifactSpec,
        include_import: bool,
    ) -> Result<StagingCleanup, CacheError> {
        let path = |ext: &str| self.root.join(format!("{}.{ext}", spec.digest));
        let try_interest = || match private_file(&path("subscriptions"), false) {
            Ok(file) => exclusive_lease(file).map(Some),
            Err(CacheError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
        let mut interest = match try_interest() {
            Ok(guard) => guard,
            Err(CacheError::LockUnavailable) => return Ok(StagingCleanup::RetainedForSubscribers),
            Err(e) => return Err(e),
        };
        let _writer = match cache_lease(&self.root.join(".model-cache-lock")) {
            Ok(guard) => guard,
            Err(CacheError::LockUnavailable) => return Ok(StagingCleanup::WriterBusy),
            Err(e) => return Err(e),
        };
        // Files from a legacy writer or a direct import may have no subscription record.
        // Avoid allocating metadata on a full volume; recheck registration under the writer lock.
        if interest.is_none() {
            interest = match try_interest() {
                Ok(guard) => guard,
                Err(CacheError::LockUnavailable) => {
                    return Ok(StagingCleanup::RetainedForSubscribers);
                }
                Err(e) => return Err(e),
            };
        }
        let mut targets = Vec::new();
        for ext in ["part", "resume", "import"] {
            if ext == "import" && !include_import {
                continue;
            }
            let name = path(ext);
            if exists(&name)? {
                let file = private_file(&name, false)?;
                targets.push((ext, name, file.metadata()?.len()));
            }
        }
        let (mut download_bytes, mut import_bytes) = (0, 0);
        for (ext, name, bytes) in targets {
            fs::remove_file(name)?;
            if ext == "part" {
                download_bytes = bytes;
            }
            if ext == "import" {
                import_bytes = bytes;
            }
        }
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        drop(interest);
        Ok(StagingCleanup::Released {
            download_bytes,
            import_bytes,
        })
    }
}
