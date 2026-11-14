//! Successful same-Run version transitions and guarded historical reads.
//! The public trusted-adapter entry rejects normal Runs. Editing Runs enter
//! only through an engine-issued witness after capability and contract checks.
use super::authority::SourceIdentity;
use super::review::{Record, dispatch::PublishedPatch};
use super::*;
use crate::workspace::historical::{HistoricalFile, HistoricalOverlay};
use crate::workspace::{
    self, CancellationGuard, READ_SLOTS, ToolReceipt, WorkspaceGrant, WorkspaceRequest,
    WorkspaceResult, directory_identity, open_directory,
};
use crate::{Journal, JournalError};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, OpenOptions};
use orange_protocol::{model::ResponseCandidate, valid_id};
use rusqlite::{OptionalExtension, params};
use std::{collections::BTreeSet, io::Read};
const MAX_EDGES: usize = 128;
#[cfg(test)]
pub(super) mod tests;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VersionEdge {
    pub(super) operation_id: String,
    pub(super) owner: String,
    pub(super) run: String,
    pub(super) policy: String,
    pub(super) read_environment: String,
    pub(super) resource: String,
    pub(super) path: String,
    pub(super) preview_digest: String,
    pub(super) receipt_digest: String,
    pub(super) original_sha256: String,
    pub(super) result_sha256: String,
    pub(super) retained_name: String,
    pub(super) before: SourceIdentity,
    pub(super) retained: SourceIdentity,
    pub(super) after: SourceIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) capture: Option<super::capture::CaptureEvidence>,
}
impl VersionEdge {
    pub(super) fn digest(&self) -> Result<String, PatchError> {
        request_digest("patch-version-edge.v1", self).map_err(|_| PatchError::Invalid)
    }
}
struct HistoricalSource {
    request: WorkspaceRequest,
    digest: Option<String>,
    event_seq: i64,
    patch_anchor: bool,
}
fn invalid() -> PatchError {
    JournalError::Conflict.into()
}
fn io_error(e: std::io::Error) -> PatchError {
    WorkspaceError::Io(e).into()
}
fn object(
    parent: &Dir,
    name: &str,
    expected: &SourceIdentity,
    sha: &str,
) -> Result<File, PatchError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = parent.open_with(name, &options).map_err(io_error)?;
    let named = authority::identity(parent, &parent.symlink_metadata(name).map_err(io_error)?)?;
    let opened = authority::identity(parent, &file.metadata().map_err(io_error)?)?;
    if named != opened {
        return Err(PatchError::VersionChanged);
    }
    if file.metadata().map_err(io_error)?.len() > MAX_TEXT as u64 {
        return Err(PatchError::Limit);
    }
    let security = orange_platform_files::security_metadata(&file).map_err(io_error)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_TEXT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > MAX_TEXT
        || hex::encode(Sha256::digest(&bytes)) != sha
        || opened != authority::identity(parent, &file.metadata().map_err(io_error)?)?
        || opened != authority::identity(parent, &parent.symlink_metadata(name).map_err(io_error)?)?
        || security != orange_platform_files::security_metadata(&file).map_err(io_error)?
    {
        return Err(PatchError::VersionChanged);
    }
    let mut secured = opened;
    secured.bind_security(security.digest());
    if &secured != expected {
        return Err(PatchError::VersionChanged);
    }
    Ok(file)
}
fn overlay(root: &Dir, edges: &[VersionEdge]) -> Result<HistoricalOverlay, PatchError> {
    let mut result = HistoricalOverlay::default();
    let mut chains: Vec<(HistoricalFile, &VersionEdge)> = Vec::new();
    for edge in edges {
        let preceding = chains
            .iter()
            .position(|(_, previous)| previous.after.object_key() == edge.before.object_key());
        if let Some(index) = preceding {
            let previous = chains[index].1;
            if previous.after != edge.before || previous.result_sha256 != edge.original_sha256 {
                return Err(PatchError::VersionChanged);
            }
        }
        let names = components(&edge.path, false)?;
        let (_, parents) = names.split_last().ok_or(PatchError::Invalid)?;
        let parent = open_directory(root, parents)?;
        if !edge.before.same_parent(&parent)? {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
        let suffix = edge
            .retained_name
            .strip_prefix(".orange-agent-tmp-patch-old-")
            .ok_or(PatchError::Invalid)?;
        if uuid::Uuid::parse_str(suffix)
            .map_err(|_| PatchError::Invalid)?
            .to_string()
            != suffix
        {
            return Err(PatchError::Invalid);
        }
        let old = object(
            &parent,
            &edge.retained_name,
            &edge.retained,
            &edge.original_sha256,
        )?;
        let security = orange_platform_files::security_metadata(&old).map_err(io_error)?;
        let accepted = match &edge.capture {
            Some(evidence) => evidence.accepts_identity(&edge.before, &edge.retained, &security),
            None => edge.before.matches_after_rename(&edge.retained),
        };
        if !accepted {
            return Err(invalid());
        }
        if let Some(index) = preceding {
            chains[index].1 = edge;
        } else {
            let bytes = old.metadata().map_err(io_error)?.len();
            chains.push((
                HistoricalFile {
                    file: old,
                    bytes,
                    sha256: edge.original_sha256.clone(),
                },
                edge,
            ));
        }
        result
            .hidden
            .entry(directory_identity(&parent)?)
            .or_default()
            .insert(edge.retained_name.clone());
        if directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)? {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
    }
    for (old, edge) in chains {
        let names = components(&edge.path, false)?;
        let (name, parents) = names.split_last().ok_or(PatchError::Invalid)?;
        let parent = open_directory(root, parents)?;
        object(&parent, name, &edge.after, &edge.result_sha256)?;
        if result.files.insert(edge.after.object_key(), old).is_some() {
            return Err(PatchError::VersionChanged);
        }
        if directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)? {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
    }
    Ok(result)
}
fn validate_anchor(
    root: &Dir,
    anchor: Option<&VersionEdge>,
    edges: &[VersionEdge],
) -> Result<(), PatchError> {
    let Some(anchor) = anchor else {
        return Ok(());
    };
    if let Some(next) = edges
        .iter()
        .find(|e| e.before.object_key() == anchor.after.object_key())
    {
        if next.before != anchor.after || next.original_sha256 != anchor.result_sha256 {
            return Err(PatchError::VersionChanged);
        }
    } else {
        let names = components(&anchor.path, false)?;
        let (name, parents) = names.split_last().ok_or(PatchError::Invalid)?;
        let parent = open_directory(root, parents)?;
        object(&parent, name, &anchor.after, &anchor.result_sha256)?;
        if directory_identity(&open_directory(root, parents)?)? != directory_identity(&parent)? {
            return Err(WorkspaceError::EnvironmentChanged.into());
        }
    }
    Ok(())
}
impl Journal {
    fn history_source(
        &self,
        owner: &str,
        run: &str,
        source: &str,
        read: &WorkspaceGrant,
        access: Option<&crate::engine::HistoryAccess>,
    ) -> Result<HistoricalSource, PatchError> {
        if ![owner, run, source].iter().all(|s| valid_id(s))
            || owner != read.owner()
            || read.is_revoked()
        {
            return Err(WorkspaceError::Forbidden.into());
        }
        let normal: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_runs WHERE run_id=?)",
                [run],
                |r| r.get(0),
            )
            .map_err(JournalError::from)?;
        if normal != access.is_some()
            || access.is_some_and(|a| !a.valid_for(owner, run, source, read))
        {
            return Err(PatchError::RecoveryRequired);
        }
        let operation_environment =
            access.map_or(read.environment(), |a| a.operation_environment());
        let row:Option<(String,String,String,String,String,String,String)>=self.connection.query_row(
            "SELECT o.request_json,o.argument_digest,o.tool_version,o.result_json,o.result_digest,p.body,o.call_id FROM operations o JOIN responses p USING(invocation) JOIN runs r ON r.run_id=o.run_id JOIN approvals a USING(operation_id) WHERE o.operation_id=? AND o.run_id=? AND o.owner=? AND o.policy=? AND o.environment=? AND o.state='succeeded' AND o.requires_approval=1 AND r.active=1 AND r.owner=o.owner AND r.policy=o.policy AND p.run_id=o.run_id AND p.attempt=1 AND p.body IS NOT NULL AND a.actor=o.owner AND a.argument_digest=o.argument_digest AND a.environment=o.environment AND a.policy=o.policy",
            params![source,run,owner,read.policy(),operation_environment],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)),
        ).optional().map_err(JournalError::from)?;
        let (request, args_digest, version, result, digest, body, call_id) =
            row.ok_or(WorkspaceError::Forbidden)?;
        if request.len() > 64 * 1024 || result.len() > 2 * 1024 * 1024 || body.len() > 256 * 1024 {
            return Err(PatchError::Limit);
        }
        let mut request: WorkspaceRequest =
            serde_json::from_str(&request).map_err(|_| PatchError::Invalid)?;
        request.validate()?;
        if (matches!(
            request,
            WorkspaceRequest::WriteFile(_) | WorkspaceRequest::ApplyPatch(_)
        ) && access.is_none())
            || request.version() != version
            || request.digest()? != args_digest
        {
            return Err(invalid());
        }
        let response: ResponseCandidate =
            serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
        let stored_digest:String=self.connection.query_row("SELECT p.digest FROM operations o JOIN responses p USING(invocation) WHERE o.operation_id=?",[source],|r|r.get(0)).map_err(JournalError::from)?;
        if request_digest("model-response-v1", &response).map_err(|_| PatchError::Invalid)?
            != stored_digest
            || response.finish_reason != "tool_calls"
            || response.tool_calls.is_empty()
            || response.tool_calls.len() > 80
            || !valid_id(&response.response_id)
        {
            return Err(invalid());
        }
        let mut ids = BTreeSet::new();
        if response
            .tool_calls
            .iter()
            .any(|call| !valid_id(&call.id) || !ids.insert(&call.id))
        {
            return Err(invalid());
        }
        let call = response
            .tool_calls
            .iter()
            .find(|call| call.id == call_id)
            .ok_or_else(invalid)?;
        if WorkspaceRequest::parse(&call.name, &call.arguments)?.digest()? != args_digest {
            return Err(invalid());
        }
        let mut expected_digest = Some(digest.clone());
        let patch_anchor = matches!(request, WorkspaceRequest::ApplyPatch(_));
        let file_source = match &request {
            WorkspaceRequest::WriteFile(args) => {
                let receipt: crate::workspace::creation::CreatedFile =
                    serde_json::from_str(&result).map_err(|_| PatchError::Invalid)?;
                if request_digest("created-file.v1", &receipt).map_err(|_| PatchError::Invalid)?
                    != digest
                    || receipt.path != args.path
                    || receipt.sha256 != hex::encode(Sha256::digest(args.content.as_bytes()))
                    || receipt.bytes != args.content.len().to_string()
                {
                    return Err(invalid());
                }
                Some((receipt.path, receipt.sha256))
            }
            WorkspaceRequest::ApplyPatch(args) => {
                let receipt: PublishedPatch =
                    serde_json::from_str(&result).map_err(|_| PatchError::Invalid)?;
                if request_digest("published-patch.v1", &receipt)
                    .map_err(|_| PatchError::Invalid)?
                    != digest
                    || receipt.path != args.path
                    || receipt.original_sha256 != args.expected_sha256
                {
                    return Err(invalid());
                }
                Some((receipt.path, receipt.result_sha256))
            }
            _ => {
                let result: WorkspaceResult =
                    serde_json::from_str(&result).map_err(|_| PatchError::Invalid)?;
                if request_digest("tool-result-v1", &result).map_err(|_| PatchError::Invalid)?
                    != digest
                {
                    return Err(invalid());
                }
                None
            }
        };
        if let Some((path, sha)) = file_source {
            request = WorkspaceRequest::ReadFile(orange_protocol::tools::ReadFileArgs {
                path,
                expected_sha256: Some(sha),
                start_byte: None,
                max_bytes: None,
                start_line: None,
                end_line: None,
            });
            expected_digest = None;
        }
        let (count,event_seq):(i64,i64)=self.connection.query_row("SELECT count(*),coalesce(min(seq),0) FROM events WHERE operation_id=? AND run_id=? AND kind='operation.succeeded'",params![source,run],|r|Ok((r.get(0)?,r.get(1)?))).map_err(JournalError::from)?;
        if count != 1 {
            return Err(invalid());
        }
        Ok(HistoricalSource {
            request,
            digest: expected_digest,
            event_seq,
            patch_anchor,
        })
    }
    fn history_edges(
        &self,
        owner: &str,
        run: &str,
        source_seq: i64,
        read: &WorkspaceGrant,
    ) -> Result<Vec<VersionEdge>, PatchError> {
        let unsafe_effect:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner=? AND run_id=? AND state IN('dispatching','unknown'))",params![owner,run],|r|r.get(0)).map_err(JournalError::from)?;
        if unsafe_effect {
            return Err(PatchError::RecoveryRequired);
        }
        let mut query=self.connection.prepare("SELECT o.operation_id,e.seq,x.body,x.digest,o.result_json,o.result_digest,p.body,p.digest FROM operations o JOIN events e ON e.operation_id=o.operation_id AND e.run_id=o.run_id AND e.kind='operation.succeeded' LEFT JOIN patch_version_edges x ON x.operation_id=o.operation_id AND x.event_seq=e.seq JOIN editing_previews p ON p.operation_id=o.operation_id WHERE o.owner=? AND o.run_id=? AND o.state='succeeded' AND o.tool_version='apply_patch.v1' AND e.seq>? ORDER BY e.seq LIMIT 129").map_err(JournalError::from)?;
        let rows = query
            .query_map(params![owner, run, source_seq], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })
            .map_err(JournalError::from)?;
        let mut result = Vec::new();
        let mut operations = BTreeSet::new();
        for row in rows {
            if result.len() >= MAX_EDGES {
                return Err(PatchError::Limit);
            }
            let (id, _, body, digest, receipt, receipt_digest, review, preview_digest) =
                row.map_err(JournalError::from)?;
            let (Some(body), Some(digest)) = (body, digest) else {
                return Err(PatchError::RecoveryRequired);
            };
            if body.len() > 32 * 1024 || receipt.len() > 16 * 1024 || review.len() > 512 * 1024 {
                return Err(PatchError::Limit);
            }
            let edge: VersionEdge = serde_json::from_str(&body).map_err(|_| PatchError::Invalid)?;
            let receipt: PublishedPatch =
                serde_json::from_str(&receipt).map_err(|_| PatchError::Invalid)?;
            let review: Record = serde_json::from_str(&review).map_err(|_| PatchError::Invalid)?;
            if !operations.insert(id.clone())
                || edge.operation_id != id
                || edge.digest()? != digest
                || edge.owner != owner
                || edge.run != run
                || edge.policy != read.policy()
                || edge.read_environment != read.environment()
                || edge.resource != read.resource_identity()
                || edge.receipt_digest != receipt_digest
                || request_digest("published-patch.v1", &receipt)
                    .map_err(|_| PatchError::Invalid)?
                    != receipt_digest
                || edge.path != receipt.path
                || edge.original_sha256 != receipt.original_sha256
                || edge.result_sha256 != receipt.result_sha256
                || edge.retained_name != receipt.retained_original
                || edge.preview_digest != preview_digest
                || request_digest("patch-review.v1", &review).map_err(|_| PatchError::Invalid)?
                    != preview_digest
                || review.operation_id != id
                || review.owner != owner
                || review.run != run
                || review.policy != read.policy()
                || review.read_environment != read.environment()
                || review.source_identity != edge.before
                || review.preview.original_sha256 != edge.original_sha256
                || review.preview.result_sha256 != edge.result_sha256
                || review.args.path != edge.path
                || edge
                    .capture
                    .as_ref()
                    .is_some_and(|e| !e.source_bound(&edge.before))
            {
                return Err(invalid());
            }
            let saved = self.capture_evidence(owner, &id)?;
            if edge.capture.as_ref().is_some_and(|e| e != &saved)
                || (edge.capture.is_none() && saved != super::capture::CaptureEvidence::default())
            {
                return Err(invalid());
            }
            result.push(edge);
        }
        Ok(result)
    }
    /// Revalidate an already committed readonly tool result for a trusted
    /// adapter. A source operation ID is mandatory; callers cannot choose a
    /// historical timestamp, fabricate an overlay or name protected files.
    pub async fn revalidate_workspace_history(
        &self,
        owner: &str,
        run: &str,
        source: &str,
        read: &WorkspaceGrant,
    ) -> Result<ToolReceipt, PatchError> {
        self.revalidate_history_with_hook(owner, run, source, read, || {})
            .await
    }
    async fn revalidate_history_with_hook(
        &self,
        owner: &str,
        run: &str,
        source: &str,
        read: &WorkspaceGrant,
        hook: impl FnOnce() + Send + 'static,
    ) -> Result<ToolReceipt, PatchError> {
        self.revalidate_history_access(owner, run, source, read, None, hook)
            .await
    }
    pub(crate) async fn revalidate_agent_history(
        &self,
        access: crate::engine::HistoryAccess,
        read: &WorkspaceGrant,
    ) -> Result<ToolReceipt, PatchError> {
        let (owner, run, source) = access.coordinates();
        self.revalidate_history_access(owner, run, source, read, Some(&access), || {})
            .await
    }
    async fn revalidate_history_access(
        &self,
        owner: &str,
        run: &str,
        source_id: &str,
        read: &WorkspaceGrant,
        access: Option<&crate::engine::HistoryAccess>,
        hook: impl FnOnce() + Send + 'static,
    ) -> Result<ToolReceipt, PatchError> {
        let source = self.history_source(owner, run, source_id, read, access)?;
        let mut edges = self.history_edges(
            owner,
            run,
            source.event_seq - if source.patch_anchor { 1 } else { 0 },
            read,
        )?;
        let anchor = if source.patch_anchor {
            if edges.first().is_none_or(|e| e.operation_id != source_id) {
                return Err(PatchError::RecoveryRequired);
            }
            Some(edges.remove(0))
        } else {
            None
        };
        let slot = READ_SLOTS
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WorkspaceError::Interrupted)?;
        let permit = read
            .permit(owner, read.environment(), read.policy())
            .await?;
        let mut cancellation = CancellationGuard {
            inner: permit.inner.clone(),
            finished: false,
        };
        let work = tokio::task::spawn_blocking(move || -> Result<ToolReceipt, PatchError> {
            let _slot = slot;
            if permit.inner.is_revoked() {
                return Err(WorkspaceError::Revoked.into());
            }
            validate_anchor(&permit.inner.directory, anchor.as_ref(), &edges)?;
            let view = overlay(&permit.inner.directory, &edges)?;
            let result = match source.request {
                WorkspaceRequest::ReadFile(args) => WorkspaceResult::ReadFile(
                    workspace::read_file_historical(&permit.inner.directory, &args, &view)?,
                ),
                WorkspaceRequest::ListDirectory(args) => {
                    WorkspaceResult::ListDirectory(workspace::list_directory_historical(
                        &permit.inner.directory,
                        &permit.inner.environment,
                        &args,
                        &view,
                    )?)
                }
                WorkspaceRequest::SearchFiles(args) => WorkspaceResult::SearchFiles(
                    workspace::search::files_historical(&permit.inner, &args, &view)?,
                ),
                WorkspaceRequest::SearchText(args) => WorkspaceResult::SearchText(
                    workspace::text_search::execute_historical(&permit.inner, &args, &view)?,
                ),
                WorkspaceRequest::WriteFile(_) | WorkspaceRequest::ApplyPatch(_) => {
                    return Err(WorkspaceError::Forbidden.into());
                }
            };
            if source.digest.as_ref().is_some_and(|expected| {
                request_digest("tool-result-v1", &result).ok().as_ref() != Some(expected)
            }) {
                return Err(PatchError::VersionChanged);
            }
            hook();
            if permit.inner.is_revoked() {
                return Err(WorkspaceError::Revoked.into());
            }
            // No bytes leave the worker before current targets and retained
            // objects are checked again, including deletion and inode changes.
            drop(view);
            drop(overlay(&permit.inner.directory, &edges)?);
            validate_anchor(&permit.inner.directory, anchor.as_ref(), &edges)?;
            if permit.inner.is_revoked() {
                return Err(WorkspaceError::Revoked.into());
            }
            let model_text = workspace::model_view(&result)?;
            Ok(ToolReceipt {
                result,
                model_text,
                _permit: permit,
            })
        });
        let result = tokio::time::timeout(read.inner.deadline, work)
            .await
            .map_err(|_| WorkspaceError::Timeout)?
            .map_err(|_| WorkspaceError::Interrupted)?;
        cancellation.finished = true;
        result
    }
}
