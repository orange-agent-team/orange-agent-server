//! Private, typed read overlay. Neither model arguments nor ordinary grants can
//! manufacture it. The lineage verifier owns both construction and validation.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Seek;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ObjectKey {
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) created: String,
}
pub(super) fn object_key(metadata: &cap_std::fs::Metadata) -> Result<ObjectKey, WorkspaceError> {
    #[cfg(target_vendor = "apple")]
    {
        use cap_fs_ext::OsMetadataExt;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.ino() == 0 {
            return Err(WorkspaceError::Forbidden);
        }
        let created = match metadata.created()?.into_std().duration_since(UNIX_EPOCH) {
            Ok(value) => format!("+{}", value.as_nanos()),
            Err(error) => format!("-{}", error.duration().as_nanos()),
        };
        Ok(ObjectKey {
            device: metadata.dev(),
            inode: metadata.ino(),
            created,
        })
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = metadata;
        Err(WorkspaceError::PersistentIdentityUnavailable)
    }
}

pub(super) struct HistoricalFile {
    pub(super) file: cap_std::fs::File,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}
#[derive(Default)]
pub(super) struct HistoricalOverlay {
    pub(super) files: BTreeMap<ObjectKey, HistoricalFile>,
    pub(super) hidden: BTreeMap<(u64, u64), BTreeSet<String>>,
}
impl HistoricalOverlay {
    pub(super) fn directory_key(&self, dir: &Dir) -> Result<Option<(u64, u64)>, WorkspaceError> {
        if self.hidden.is_empty() {
            Ok(None)
        } else {
            Ok(Some(directory_identity(dir)?))
        }
    }
    pub(super) fn hidden(&self, directory: Option<(u64, u64)>, name: &str) -> bool {
        directory
            .and_then(|id| self.hidden.get(&id))
            .is_some_and(|names| names.contains(name))
    }
    pub(super) fn extra_scan_allowance(&self) -> usize {
        self.hidden.values().map(BTreeSet::len).sum()
    }
    pub(super) fn size(&self, metadata: &cap_std::fs::Metadata) -> Result<u64, WorkspaceError> {
        if self.files.is_empty() {
            return Ok(metadata.len());
        }
        Ok(self
            .files
            .get(&object_key(metadata)?)
            .map_or(metadata.len(), |file| file.bytes))
    }
    pub(super) fn read(
        &self,
        root: &Dir,
        path: &str,
        maximum: usize,
        expected: Option<&str>,
    ) -> Result<(Vec<u8>, String), WorkspaceError> {
        let names = components(path, false)?;
        if self.files.is_empty() {
            return read_bytes(root, path, maximum, expected);
        }
        let (name, parents) = names.split_last().ok_or(WorkspaceError::Invalid)?;
        let directory = open_directory(root, parents)?;
        let metadata = directory.symlink_metadata(name)?;
        let Some(old) = self.files.get(&object_key(&metadata)?) else {
            return read_bytes(root, path, maximum, expected);
        };
        if old.bytes > maximum.min(MAX_FILE) as u64 {
            return Err(WorkspaceError::FileTooLarge);
        }
        let mut file = old.file.try_clone()?;
        file.rewind()?;
        let mut bytes = Vec::new();
        file.take((maximum.min(MAX_FILE) + 1) as u64)
            .read_to_end(&mut bytes)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        if bytes.len() as u64 != old.bytes
            || digest != old.sha256
            || expected.is_some_and(|e| e != digest)
        {
            return Err(WorkspaceError::VersionChanged);
        }
        Ok((bytes, digest))
    }
}
