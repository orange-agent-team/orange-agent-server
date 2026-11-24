//! Explicit user-selected local artifact import into the same verified cache.
//! Never links the source and never invokes a network transport.
use super::*;
use tokio::sync::watch;

fn check_cancel(cancel: &watch::Receiver<bool>) -> Result<(), CacheError> {
    if *cancel.borrow() || cancel.has_changed().is_err() {
        Err(CacheError::Cancelled)
    } else {
        Ok(())
    }
}
impl ModelCache {
    pub async fn import_file(
        &self,
        spec: &ArtifactSpec,
        source: &Path,
    ) -> Result<VerifiedArtifact, CacheError> {
        let (_keep, receiver) = watch::channel(false);
        self.import_file_with_cancel(spec, source, receiver).await
    }
    /// Caller holds its OS file-selection capability until this future finishes.
    /// Cancellation before publication leaves no active new model. If publication
    /// already won, the caller must inspect the returned/cache state; it cannot
    /// infer that a cancellation request removes a verified existing artifact.
    pub async fn import_file_with_cancel(
        &self,
        spec: &ArtifactSpec,
        source: &Path,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<VerifiedArtifact, CacheError> {
        check_cancel(&cancel)?;
        let ready = self.root.join(format!("{}.gguf", spec.digest));
        let staged = self.root.join(format!("{}.import", spec.digest));
        if exists(&ready)? {
            let file = verify(&ready, spec).await?;
            check_cancel(&cancel)?;
            return artifact(&ready, spec, true, file);
        }
        if !source.is_absolute() || source.is_symlink() {
            return Err(CacheError::UnsafePath);
        }
        if source.canonicalize()?.starts_with(&self.root) {
            // A caller-selected source must never alias a staging file that we
            // will truncate, including through an alternate spelling of root.
            return Err(CacheError::UnsafePath);
        }
        let original = private_file(source, false)?;
        if original.metadata()?.len() != spec.bytes {
            return Err(CacheError::Integrity);
        }
        let lock = cache_lease(&self.root.join(".model-cache-lock"))?;
        if exists(&ready)? {
            let file = verify(&ready, spec).await?;
            check_cancel(&cancel)?;
            return artifact(&ready, spec, true, file);
        }
        let previous = if exists(&staged)? {
            private_file(&staged, false)?.metadata()?.len()
        } else {
            0
        };
        if previous > spec.bytes {
            return Err(CacheError::TooLarge);
        }
        if self
            .used_bytes()?
            .checked_add(spec.bytes - previous)
            .is_none_or(|used| used > self.quota)
        {
            return Err(CacheError::Quota);
        }
        let result=async {
        let destination = Arc::new(Mutex::new(private_file(&staged, true)?));
        disk_io(destination.clone(), lock.clone(), |file| {
            file.set_len(0)?;
            file.seek(io::SeekFrom::Start(0))?;
            Ok(())
        })
        .await?;
        let mut source = tokio::fs::File::from_std(original);
        let mut count = 0_u64;
        let mut digest = Sha256::new();
        let mut magic = Vec::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            check_cancel(&cancel)?;
            let read = tokio::select! {
                _=cancel.wait_for(|flag|*flag)=>return Err(CacheError::Cancelled),
                result=source.read(&mut buffer)=>result?,
            };
            if read == 0 {
                break;
            }
            if read as u64 > spec.bytes - count {
                return Err(CacheError::TooLarge);
            }
            if magic.len() < spec.magic.len() {
                let needed = (spec.magic.len() - magic.len()).min(read);
                magic.extend_from_slice(&buffer[..needed]);
            }
            digest.update(&buffer[..read]);
            let chunk = buffer[..read].to_vec();
            disk_io(destination.clone(), lock.clone(), move |file| {
                file.write_all(&chunk)
            })
            .await?;
            count += read as u64;
        }
        if count != spec.bytes
            || magic != spec.magic
            || hex::encode(digest.finalize()) != spec.digest
        {
            disk_io(destination.clone(), lock.clone(), |file| {
                file.set_len(0)?;
                file.sync_all()
            })
            .await?;
            return Err(CacheError::Integrity);
        }
        disk_io(destination.clone(), lock.clone(), |file| file.sync_all()).await?;
        drop(destination);
        // Re-read our staging inode; the active model is never an alias of the
        // caller-selected file and never accepts only a claimed source digest.
        let file=tokio::select!{biased;_=cancel.wait_for(|flag|*flag)=>return Err(CacheError::Cancelled),result=verify(&staged,spec)=>result?};
        check_cancel(&cancel)?;
        fs::rename(&staged, &ready)?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        artifact(&ready, spec, false, file)
        }.await;
        if matches!(result, Err(CacheError::Cancelled)) && exists(&staged)? {
            // The cache writer lock is still held. Only the local import staging file belongs to this operation.
            fs::remove_file(&staged)?;
            #[cfg(unix)]
            File::open(&self.root)?.sync_all()?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(bytes: &[u8]) -> ArtifactSpec {
        ArtifactSpec::new(
            "https://models.invalid/pinned.gguf",
            &hex::encode(Sha256::digest(bytes)),
            bytes.len() as u64,
            &["models.invalid"],
            b"GGUF",
        )
        .expect("fixed test descriptor")
    }
    #[tokio::test]
    async fn selected_model_is_copied_verified_and_independent_of_source_changes() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("selected.gguf");
        let bytes = b"GGUF-local-test-artifact";
        fs::write(&source, bytes).expect("source");
        let cache = ModelCache::new(&temp.path().join("private"), 1024).expect("cache");
        let spec = spec(bytes);
        let artifact = cache
            .import_file(&spec, &source)
            .await
            .expect("actual import without network");
        assert!(!artifact.is_cached());
        assert_eq!(fs::read(artifact.path()).expect("cache bytes"), bytes);
        fs::write(&source, b"unrelated source replacement").expect("replace source");
        assert_eq!(fs::read(artifact.path()).expect("immutable copy"), bytes);
        assert!(
            cache
                .import_file(&spec, &source)
                .await
                .expect("verified existing cache")
                .is_cached()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(artifact.path()).expect("metadata");
            assert_eq!(metadata.nlink(), 1);
            assert_eq!(metadata.mode() & 0o077, 0);
        }
    }
    #[tokio::test]
    async fn corrupt_quota_locked_and_cancelled_imports_never_publish() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("selected.gguf");
        let bytes = b"GGUF-local-test-artifact";
        let spec = spec(bytes);
        fs::write(&source, b"GGUF-corrupt-test-bytes!").expect("bad bytes");
        let cache = ModelCache::new(&temp.path().join("private"), 1024).expect("cache");
        assert!(matches!(
            cache.import_file(&spec, &source).await,
            Err(CacheError::Integrity)
        ));
        let ready = cache.root.join(format!("{}.gguf", spec.digest));
        assert!(!ready.exists());
        fs::write(&source, bytes).expect("good source");
        let small = ModelCache::new(&temp.path().join("small"), 1).expect("small cache");
        assert!(matches!(
            small.import_file(&spec, &source).await,
            Err(CacheError::Quota)
        ));
        let lock = private_file(&cache.root.join(".model-cache-lock"), true).expect("lock");
        lock.try_lock().expect("held lock");
        assert!(matches!(
            cache.import_file(&spec, &source).await,
            Err(CacheError::LockUnavailable)
        ));
        drop(lock);
        let (sender, receiver) = watch::channel(true);
        assert!(matches!(
            cache
                .import_file_with_cancel(&spec, &source, receiver)
                .await,
            Err(CacheError::Cancelled)
        ));
        drop(sender);
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        assert!(matches!(
            cache
                .import_file_with_cancel(&spec, &source, receiver)
                .await,
            Err(CacheError::Cancelled)
        ));
        assert!(!ready.exists());
        cache
            .import_file(&spec, &source)
            .await
            .expect("retry after actual failures");
    }
    #[tokio::test]
    async fn selecting_owned_staging_as_source_never_truncates_it() {
        let temp = tempfile::tempdir().expect("temp");
        let bytes = b"GGUF-local-test-artifact";
        let spec = spec(bytes);
        let cache = ModelCache::new(&temp.path().join("private"), 1024).expect("cache");
        let staged = cache.root.join(format!("{}.import", spec.digest));
        fs::write(&staged, bytes).expect("existing partial fixture");
        assert!(matches!(
            cache.import_file(&spec, &staged).await,
            Err(CacheError::UnsafePath)
        ));
        assert_eq!(fs::read(&staged).expect("source preserved"), bytes);
        assert!(!cache.root.join(format!("{}.gguf", spec.digest)).exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn linked_or_nonregular_source_cannot_block_or_redirect_an_import() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("selected.gguf");
        let bytes = b"GGUF-local-test-artifact";
        let spec = spec(bytes);
        fs::write(&source, bytes).expect("source");
        let cache = ModelCache::new(&temp.path().join("private"), 1024).expect("cache");
        let linked = temp.path().join("linked.gguf");
        symlink(&source, &linked).expect("symlink");
        assert!(matches!(
            cache.import_file(&spec, &linked).await,
            Err(CacheError::UnsafePath)
        ));
        fs::remove_file(&linked).expect("remove test link");
        fs::hard_link(&source, &linked).expect("hardlink");
        assert!(matches!(
            cache.import_file(&spec, &linked).await,
            Err(CacheError::UnsafePath)
        ));
        assert!(matches!(
            cache.import_file(&spec, temp.path()).await,
            Err(CacheError::UnsafePath)
        ));
        let fifo = temp.path().join("pipe.gguf");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("fixture FIFO")
                .success()
        );
        assert!(matches!(
            cache.import_file(&spec, &fifo).await,
            Err(CacheError::UnsafePath)
        ));
    }
}
