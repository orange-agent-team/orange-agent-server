//! The local filesystem and versioned object store share the same transfer state machine.
use super::storage::LocalObjects;
use super::versioned_store::VersionedStore;
use super::*;

pub(super) struct ObjectBackend {
    pub root_id: String,
    backend: Backend,
}
enum Backend {
    Local(LocalObjects),
    Versioned(Box<VersionedStore>),
}

impl From<LocalObjects> for ObjectBackend {
    fn from(local: LocalObjects) -> Self {
        Self {
            root_id: local.root_id.clone(),
            backend: Backend::Local(local),
        }
    }
}
impl From<VersionedStore> for ObjectBackend {
    fn from(store: VersionedStore) -> Self {
        Self {
            root_id: store.root_id.clone(),
            backend: Backend::Versioned(Box::new(store)),
        }
    }
}
impl ObjectBackend {
    pub async fn writer(&self, key: &str) -> Result<opendal::Writer, ArtifactError> {
        match &self.backend {
            Backend::Local(store) => store.writer(key).await,
            Backend::Versioned(store) => store.writer(key).await,
        }
    }
    pub async fn finish_write(
        &self,
        key: &str,
        receipt: opendal::Metadata,
    ) -> Result<(), ArtifactError> {
        match &self.backend {
            Backend::Local(store) => store.finish_write(key),
            Backend::Versioned(store) => store.pin(key, &receipt).await,
        }
    }
    pub async fn reader(&self, key: &str) -> Result<opendal::Reader, ArtifactError> {
        match &self.backend {
            Backend::Local(store) => Ok(store.op.reader(key).await?),
            Backend::Versioned(store) => store.reader(key).await,
        }
    }
    pub fn check_file(&self, key: &str) -> Result<(), ArtifactError> {
        match &self.backend {
            Backend::Local(store) => store.check_file(key),
            Backend::Versioned(_) => super::storage::key_valid(key),
        }
    }
    pub async fn read(&self, key: &str, size: u64) -> Result<opendal::Buffer, ArtifactError> {
        match &self.backend {
            Backend::Local(store) => store.read(key, size).await,
            Backend::Versioned(store) => store.read(key, size, 0..size).await,
        }
    }
    pub async fn verified_audio_chunk(
        &self,
        key: &str,
        size: u64,
        start: u64,
        expected: &str,
    ) -> Result<Vec<u8>, ArtifactError> {
        match &self.backend {
            Backend::Local(store) => store.verified_audio_chunk(key, size, start, expected).await,
            Backend::Versioned(store) => {
                let chunk = orange_protocol::pricing::AUDIO_INPUT_CHUNK_BYTES as u64;
                if size > orange_protocol::artifacts::MEDIA_LIMIT
                    || start >= size
                    || !start.is_multiple_of(chunk)
                {
                    return Err(ArtifactError::Invalid);
                }
                let data = store
                    .read(key, size, start..(start + chunk).min(size))
                    .await?
                    .to_vec();
                if hex::encode(Sha256::digest(&data)) != expected {
                    return Err(ArtifactError::Integrity);
                }
                Ok(data)
            }
        }
    }
    pub async fn remove(&self, key: &str) -> Result<(), ArtifactError> {
        match &self.backend {
            Backend::Local(store) => store.remove(key).await,
            Backend::Versioned(store) => store.remove(key).await,
        }
    }
    pub async fn copy_pinned(
        &self,
        stage: &str,
        published: &str,
        size: u64,
        digest: &str,
    ) -> Result<bool, ArtifactError> {
        match &self.backend {
            Backend::Local(_) => Ok(false),
            Backend::Versioned(store) => {
                store.copy(stage, published, size, digest).await?;
                Ok(true)
            }
        }
    }
    pub fn remote(&self) -> Result<&VersionedStore, ArtifactError> {
        match &self.backend {
            Backend::Versioned(store) => Ok(store),
            Backend::Local(_) => Err(ArtifactError::DirectUnavailable),
        }
    }
}
