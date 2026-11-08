//! Exact single-file unified-hunk preparation. No filesystem effect and no
//! permission grant: the eventual executor must read/version-check through its
//! own capability and obtain a separate immutable edit approval.
#[cfg(unix)]
pub mod authority;
#[cfg(unix)]
pub mod review;
use super::{WorkspaceError, components};
use orange_protocol::{request_digest, valid_digest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
const MAX_TEXT: usize = 1024 * 1024;
const MAX_PATCH: usize = 64 * 1024;
const MAX_HUNKS: usize = 128;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchArgs {
    pub path: String,
    pub expected_sha256: String,
    pub patch: String,
}
impl PatchArgs {
    pub fn validate(&self) -> Result<(), PatchError> {
        components(&self.path, false)?;
        if !valid_digest(&self.expected_sha256) {
            return Err(PatchError::Invalid);
        }
        if self.patch.is_empty() || self.patch.len() > MAX_PATCH {
            return Err(PatchError::Limit);
        }
        if self.patch.contains('\0') || !self.patch.ends_with('\n') {
            return Err(PatchError::Invalid);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String, PatchError> {
        self.validate()?;
        request_digest(
            "apply_patch.v1",
            &serde_json::json!({"tool":"apply_patch","arguments":self}),
        )
        .map_err(|_| PatchError::Invalid)
    }
}
#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error(transparent)]
    Journal(#[from] crate::JournalError),
    #[error("unresolved execution or permission state requires recovery")]
    RecoveryRequired,
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("invalid single-file unified patch")]
    Invalid,
    #[error("patch input or result exceeds its bound")]
    Limit,
    #[error("source version differs from the reviewed base")]
    VersionChanged,
    #[error("patch context does not exactly match the source")]
    ContextChanged,
    #[error("only bounded UTF-8 text without NUL is supported")]
    NotText,
    #[error("patch makes no change")]
    NoChange,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PatchPreview {
    pub path: String,
    pub argument_digest: String,
    pub original_sha256: String,
    pub result_sha256: String,
    pub original_bytes: String,
    pub result_bytes: String,
    pub hunks: u32,
    pub added_lines: u32,
    pub removed_lines: u32,
    /// Display-only headers and the exact reviewed hunk body. Never a command.
    pub diff: String,
}
pub struct PreparedPatch {
    preview: PatchPreview,
    content: Vec<u8>,
}
impl PreparedPatch {
    pub fn preview(&self) -> &PatchPreview {
        &self.preview
    }
    pub fn content(&self) -> &[u8] {
        &self.content
    }
}
struct Record {
    kind: u8,
    text: String,
    no_newline: bool,
}
struct Hunk {
    old_index: usize,
    old_count: usize,
    new_index: usize,
    new_count: usize,
    records: Vec<Record>,
}
fn number(value: &str) -> Result<usize, PatchError> {
    if value.is_empty() || value.len() > 8 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PatchError::Invalid);
    }
    let n = value.parse::<usize>().map_err(|_| PatchError::Invalid)?;
    if n > MAX_TEXT + 1 {
        return Err(PatchError::Limit);
    }
    Ok(n)
}
fn range(value: &str) -> Result<(usize, usize), PatchError> {
    let (start, count) = if let Some((start, count)) = value.split_once(',') {
        (number(start)?, number(count)?)
    } else {
        (number(value)?, 1)
    };
    let index = if count == 0 {
        start
    } else {
        start.checked_sub(1).ok_or(PatchError::Invalid)?
    };
    Ok((index, count))
}
fn header(line: &str) -> Result<Hunk, PatchError> {
    let line = line.strip_suffix('\n').ok_or(PatchError::Invalid)?;
    let ranges = line.strip_prefix("@@ -").ok_or(PatchError::Invalid)?;
    let (old, remainder) = ranges.split_once(" +").ok_or(PatchError::Invalid)?;
    let (new, label) = remainder.split_once(" @@").ok_or(PatchError::Invalid)?;
    if !label.is_empty() && !label.starts_with(' ') {
        return Err(PatchError::Invalid);
    }
    let (old_index, old_count) = range(old)?;
    let (new_index, new_count) = range(new)?;
    if old_count == 0 && new_count == 0 {
        return Err(PatchError::Invalid);
    }
    Ok(Hunk {
        old_index,
        old_count,
        new_index,
        new_count,
        records: Vec::new(),
    })
}
fn parse(patch: &str) -> Result<Vec<Hunk>, PatchError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in patch.split_inclusive('\n') {
        if line.starts_with("@@") {
            if hunks.len() == MAX_HUNKS {
                return Err(PatchError::Limit);
            }
            hunks.push(header(line)?);
            continue;
        }
        let hunk = hunks.last_mut().ok_or(PatchError::Invalid)?;
        if line == "\\ No newline at end of file\n" {
            let record = hunk.records.last_mut().ok_or(PatchError::Invalid)?;
            if record.no_newline || !record.text.ends_with('\n') {
                return Err(PatchError::Invalid);
            }
            record.text.pop();
            if record.text.is_empty() {
                return Err(PatchError::Invalid);
            }
            record.no_newline = true;
            continue;
        }
        let Some(kind) = line.as_bytes().first().copied() else {
            return Err(PatchError::Invalid);
        };
        if !matches!(kind, b' ' | b'+' | b'-') {
            return Err(PatchError::Invalid);
        }
        hunk.records.push(Record {
            kind,
            text: line[1..].into(),
            no_newline: false,
        });
    }
    if hunks.is_empty() {
        return Err(PatchError::Invalid);
    }
    for hunk in &hunks {
        let old = hunk.records.iter().filter(|r| r.kind != b'+').count();
        let new = hunk.records.iter().filter(|r| r.kind != b'-').count();
        if old != hunk.old_count || new != hunk.new_count {
            return Err(PatchError::Invalid);
        }
    }
    Ok(hunks)
}
/// Pure preparation, not proof that a path is currently owned or unchanged.
/// Execution must revalidate actual file identity/digest before any effect.
pub fn prepare(args: &PatchArgs, original: &[u8]) -> Result<PreparedPatch, PatchError> {
    args.validate()?;
    if original.len() > MAX_TEXT {
        return Err(PatchError::Limit);
    }
    let text = std::str::from_utf8(original).map_err(|_| PatchError::NotText)?;
    if text.contains('\0') {
        return Err(PatchError::NotText);
    }
    let original_hash = hex::encode(Sha256::digest(original));
    if original_hash != args.expected_sha256 {
        return Err(PatchError::VersionChanged);
    }
    let hunks = parse(&args.patch)?;
    let source: Vec<&str> = text.split_inclusive('\n').collect();
    let mut result: Vec<&str> = Vec::new();
    let mut cursor = 0;
    let mut bytes = 0usize;
    let mut previous: Option<(usize, usize)> = None;
    let mut added = 0u32;
    let mut removed = 0u32;
    fn append<'a>(
        result: &mut Vec<&'a str>,
        bytes: &mut usize,
        value: &'a str,
    ) -> Result<(), PatchError> {
        *bytes = bytes.checked_add(value.len()).ok_or(PatchError::Limit)?;
        if *bytes > MAX_TEXT {
            return Err(PatchError::Limit);
        }
        result.push(value);
        Ok(())
    }
    for hunk in &hunks {
        if hunk.old_index < cursor
            || hunk.old_index > source.len()
            || previous.is_some_and(|(index, count)| count == 0 && index == hunk.old_index)
        {
            return Err(PatchError::Invalid);
        }
        for line in &source[cursor..hunk.old_index] {
            append(&mut result, &mut bytes, line)?;
        }
        cursor = hunk.old_index;
        if result.len() != hunk.new_index {
            return Err(PatchError::Invalid);
        }
        for record in &hunk.records {
            if record.kind != b'+' {
                if source.get(cursor).copied() != Some(record.text.as_str()) {
                    return Err(PatchError::ContextChanged);
                }
                cursor += 1;
            }
            if record.kind != b'-' {
                append(&mut result, &mut bytes, &record.text)?;
            }
            if record.kind == b'+' {
                added += 1;
            }
            if record.kind == b'-' {
                removed += 1;
            }
        }
        previous = Some((hunk.old_index, hunk.old_count));
    }
    for line in &source[cursor..] {
        append(&mut result, &mut bytes, line)?;
    }
    if result
        .iter()
        .take(result.len().saturating_sub(1))
        .any(|line| !line.ends_with('\n'))
    {
        return Err(PatchError::Invalid);
    }
    let content = result.concat().into_bytes();
    if content == original {
        return Err(PatchError::NoChange);
    }
    let result_hash = hex::encode(Sha256::digest(&content));
    let diff = format!(
        "--- {:?} sha256={}\n+++ {:?} sha256={}\n{}",
        args.path, original_hash, args.path, result_hash, args.patch
    );
    let preview = PatchPreview {
        path: args.path.clone(),
        argument_digest: args.digest()?,
        original_sha256: original_hash,
        result_sha256: result_hash,
        original_bytes: original.len().to_string(),
        result_bytes: content.len().to_string(),
        hunks: u32::try_from(hunks.len()).map_err(|_| PatchError::Limit)?,
        added_lines: added,
        removed_lines: removed,
        diff,
    };
    Ok(PreparedPatch { preview, content })
}
#[cfg(test)]
mod tests;
