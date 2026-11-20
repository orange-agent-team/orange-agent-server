use super::*;
type Client = host::run_store_client::RunStoreClient<Channel>;
pub(super) struct Dispatch {
    operation: String,
    permit: String,
    version: u64,
    argument_digest: String,
    capability: String,
    workspace: String,
    allocation: String,
}
pub(super) async fn next(
    client: &mut Client,
    a: &host::RunAuthority,
    journal: &Journal,
    e: &mut Entry,
    lease: &host::WorkspaceLease,
    session: &mut Session,
) -> Result<Option<Dispatch>, Error> {
    let work = client
        .pending_operations(a.clone())
        .await?
        .into_inner()
        .tools;
    if work.is_empty() {
        return Ok(None);
    }
    if work.len() != 1 {
        return Err(Error::Recovery);
    }
    let item = work.into_iter().next().ok_or(Error::Recovery)?;
    let intent = item.intent.ok_or(Error::Recovery)?;
    if intent.authority.as_ref() != Some(a)
        || intent.tool_name != crate::cloud_tools::NAME
        || intent.execution_environment_id != lease.workspace_id
        || !valid_digest(&intent.operation_id)
        || !valid_digest(&intent.approval_id)
    {
        return Err(Error::Recovery);
    }
    let args: Value = serde_json::from_slice(&item.arguments_json).map_err(|_| Error::Recovery)?;
    let limit = crate::cloud_tools::arguments(&args).map_err(|_| Error::Recovery)?;
    if orange_protocol::request_digest("cloud.tool-arguments.v1", &args)
        .map_err(|_| Error::Recovery)?
        != intent.argument_digest
    {
        return Err(Error::Recovery);
    }
    let op = if let Some(op) = item.record {
        op
    } else {
        client.append_intent(intent.clone()).await?.into_inner()
    };
    if op.run_id != e.run
        || op.operation_id != intent.operation_id
        || op.argument_digest != intent.argument_digest
        || op.approval_id != intent.approval_id
    {
        return Err(Error::Recovery);
    }
    if op.state != host::operation_record::State::Intended as i32 || op.approval_state != "approved"
    {
        return Ok(None);
    }
    if e.dispatched_operation.is_some() {
        return Err(Error::Recovery);
    }
    let issued = client
        .acquire_dispatch(host::DispatchRequest {
            authority: Some(a.clone()),
            operation_id: op.operation_id.clone(),
            expected_version: op.version,
            argument_digest: op.argument_digest.clone(),
        })
        .await?
        .into_inner();
    let sent = issued.operation.ok_or(Error::Recovery)?;
    if issued.authority.as_ref() != Some(a)
        || sent.operation_id != op.operation_id
        || sent.state != host::operation_record::State::Dispatching as i32
        || sent.argument_digest != op.argument_digest
        || sent.version != op.version.checked_add(1).ok_or(Error::Recovery)?
        || !valid_id(&issued.permit_id)
    {
        return Err(Error::Recovery);
    }
    let expires = issued.expires_at.ok_or(Error::Recovery)?;
    if expires.nanos != 0 || expires.seconds <= time::OffsetDateTime::now_utc().unix_timestamp() {
        return Err(Error::Recovery);
    }
    e.dispatched_operation = Some(op.operation_id.clone());
    journal.save(e)?;
    session.directory(&json!({"operation_id":op.operation_id,"permit_id":issued.permit_id,"argument_digest":op.argument_digest,
        "allocation_id":lease.allocation_id,"workspace_epoch":lease.epoch,"run_epoch":a.run_epoch,"expires_at":expires.seconds,"limit":limit})).await?;
    Ok(Some(Dispatch {
        operation: op.operation_id,
        permit: issued.permit_id,
        version: sent.version,
        argument_digest: op.argument_digest,
        capability: intent.capability_snapshot_id,
        workspace: lease.workspace_id.clone(),
        allocation: lease.allocation_id.clone(),
    }))
}
pub(super) async fn commit(
    client: &mut Client,
    a: &host::RunAuthority,
    d: &Dispatch,
    v: &Value,
) -> Result<(), Error> {
    if v["operation_id"] != d.operation
        || v["permit_id"] != d.permit
        || v["run_epoch"].as_u64() != Some(a.run_epoch)
        || v["allocation_id"] != d.allocation
        || v["argument_digest"] != d.argument_digest
    {
        return Err(Error::Recovery);
    }
    let (state, result, digest) = if v["phase"] == "directory_result" {
        let entries = &v["entries"];
        let snapshot = orange_protocol::request_digest(
            "cloud.workspace-directory.v1",
            &(&d.workspace, &d.capability, entries),
        )
        .map_err(|_| Error::Recovery)?;
        let value = json!({"path":".","snapshot_digest":snapshot,"entries":entries,"next_cursor":null,"omitted_unsupported":v["omitted"]});
        let value = crate::cloud_tools::pack_directory(value).map_err(|_| Error::ToolEvidence)?;
        let digest = orange_protocol::request_digest("cloud.directory-result.v1", &value)
            .map_err(|_| Error::Recovery)?;
        (
            host::operation_record::State::Succeeded,
            serde_json::to_vec(&value).map_err(|_| Error::Recovery)?,
            digest,
        )
    } else {
        (host::operation_record::State::Unknown, Vec::new(), hash(v)?)
    };
    let request = host::ResultRequest {
        authority: Some(a.clone()),
        operation_id: d.operation.clone(),
        expected_version: d.version,
        result_state: state as i32,
        result_artifact_id: String::new(),
        result_digest: digest.clone(),
        result_json: result,
        permit_id: d.permit.clone(),
    };
    let out = match client.commit_result(request.clone()).await {
        Ok(response) => response.into_inner(),
        Err(e)
            if matches!(
                e.code(),
                tonic::Code::Unavailable
                    | tonic::Code::Cancelled
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::FailedPrecondition
            ) =>
        {
            let original = client
                .get_operation_receipt(host::OperationLookup {
                    host_instance_id: a.host_instance_id.clone(),
                    run_id: a.run_id.clone(),
                    workspace_id: d.workspace.clone(),
                    allocation_id: d.allocation.clone(),
                    run_epoch: a.run_epoch,
                    operation_id: d.operation.clone(),
                    permit_id: d.permit.clone(),
                })
                .await?
                .into_inner();
            if original.state == host::operation_record::State::Dispatching as i32 {
                client.commit_result(request).await?.into_inner()
            } else {
                original
            }
        }
        Err(e) => return Err(e.into()),
    };
    if out.operation_id == d.operation
        && out.run_id == a.run_id
        && out.state == host::operation_record::State::Unknown as i32
        && state == host::operation_record::State::Succeeded
    {
        return Err(Error::UnknownEffect);
    }
    if out.result_digest != digest {
        return Err(Error::Recovery);
    }
    if out.operation_id != d.operation || out.run_id != a.run_id || out.state != state as i32 {
        return Err(Error::Recovery);
    }
    Ok(())
}
