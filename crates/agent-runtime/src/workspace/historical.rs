//! Private, typed read overlay. Neither model arguments nor ordinary grants can
//! manufacture it. The lineage verifier owns both construction and validation.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Seek;

pub(super) struct HistoricalFile {
    pub(super) file: cap_std::fs::File,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}
#[derive(Default)]
pub(super) struct HistoricalOverlay {
    pub(super) files: BTreeMap<String, HistoricalFile>,
    pub(super) hidden: BTreeSet<String>,
}
pub(super) fn joined(parent: &str, name: &str) -> String {
    if name.is_empty() {
        parent.into()
    } else if parent == "." || parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    }
}
impl HistoricalOverlay {
    pub(super) fn hidden(&self, path: &str) -> bool {
        self.hidden.contains(path)
    }
    pub(super) fn extra_scan_allowance(&self) -> usize {
        self.hidden.len()
    }
    pub(super) fn size(&self, path: &str, current: u64) -> u64 {
        self.files.get(path).map_or(current, |file| file.bytes)
    }
    pub(super) fn read(
        &self,
        root: &Dir,
        path: &str,
        maximum: usize,
        expected: Option<&str>,
    ) -> Result<(Vec<u8>, String), WorkspaceError> {
        components(path, false)?;
        let Some(old) = self.files.get(path) else {
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
