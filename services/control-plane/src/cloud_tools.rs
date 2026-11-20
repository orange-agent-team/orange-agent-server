//! A separately pinned hosted implementation of the shared E03 directory contract.
use orange_agent_runtime::catalog::{OutputChannel, ToolCatalog};
use orange_protocol::{model::ToolDefinition, tool_spec::ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub(crate) const NAME: &str = "list_directory";
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Choice {
    pub name: String,
    pub execution_digest: String,
}
struct HostedCatalog {
    tools: ToolCatalog,
    spec: ToolSpec,
}
fn catalog() -> Result<&'static HostedCatalog, ()> {
    static CATALOG: OnceLock<Result<HostedCatalog, ()>> = OnceLock::new();
    CATALOG.get_or_init(||{
        let shared=ToolCatalog::builtin(false,false).map_err(|_|())?;
        let mut spec=shared.describe(NAME).ok_or(())?.clone();
        spec.tool_version="list_directory.hosted.v1".into();
        spec.execution_environment="hosted_workspace".into();spec.approval_required=true;spec.concurrency="exclusive".into();
        spec.description="List the prepared workspace root. Set path to a single dot. User approval is required.".into();
        spec.input_schema=json!({"type":"object","properties":{"path":{"type":"string","enum":["."]},"limit":{"type":"integer","minimum":1,"maximum":16}},
            "required":["path"],"additionalProperties":false});
        let tools=ToolCatalog::new(vec![spec.clone()]).map_err(|_|())?;Ok(HostedCatalog{tools,spec})
    }).as_ref().map_err(|_|())
}
pub(crate) fn stamp() -> Result<(String, String, String), ()> {
    let c = catalog()?;
    let p = c.tools.pin(NAME).map_err(|_| ())?;
    Ok((
        p.tool_version,
        p.execution_digest,
        c.tools.revision().into(),
    ))
}
pub(crate) fn definition() -> Result<ToolDefinition, ()> {
    catalog()?.spec.model_definition().map_err(|_| ())
}
pub(crate) fn public() -> Result<Value, ()> {
    let (version, digest, policy) = stamp()?;
    Ok(
        json!({"name":NAME,"tool_version":version,"execution_digest":digest,"policy_version":policy,
        "approval_required":true,"execution_environment":"hosted_workspace","model_definition":definition()?}),
    )
}
pub(crate) fn arguments(value: &Value) -> Result<u16, ()> {
    let c = catalog()?;
    c.tools
        .validate_input(&c.tools.pin(NAME).map_err(|_| ())?, value)
        .map_err(|_| ())?;
    let args: orange_protocol::tools::ListDirectoryArgs =
        serde_json::from_value(value.clone()).map_err(|_| ())?;
    if args.path != "." || args.cursor.is_some() {
        return Err(());
    }
    Ok(args.limit.unwrap_or(16))
}
pub(crate) fn pack_directory(data: Value) -> Result<Value, ()> {
    let result: orange_protocol::tools::ListDirectoryResult =
        serde_json::from_value(data).map_err(|_| ())?;
    let tagged = serde_json::to_value(
        orange_agent_runtime::workspace::WorkspaceResult::ListDirectory(result),
    )
    .map_err(|_| ())?;
    verify_output(&tagged)?;
    Ok(tagged)
}
pub(crate) fn verify_output(value: &Value) -> Result<(), ()> {
    let c = catalog()?;
    c.tools
        .validate_output(
            &c.tools.pin(NAME).map_err(|_| ())?,
            value,
            OutputChannel::Durable,
        )
        .map_err(|_| ())
}

pub(crate) async fn fence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run: &str,
    reason: &str,
) -> Result<(), sqlx::Error> {
    use sqlx::Row;
    let rows=sqlx::query("SELECT s.operation_id,s.state FROM cloud_operation_intents i JOIN cloud_operation_state s USING(operation_id)
        WHERE i.run_id=$1 AND s.state IN ('intended','dispatching') ORDER BY s.operation_id FOR UPDATE OF s")
        .bind(run).fetch_all(&mut **tx).await?;
    for row in rows {
        let op: String = row.try_get("operation_id")?;
        let sent = row.try_get::<String, _>("state")? == "dispatching";
        if sent {
            let proof =
                orange_protocol::request_digest("cloud.tool-unknown.v1", &(run, &op, reason))
                    .map_err(|_| sqlx::Error::Protocol("unknown disposition".into()))?;
            sqlx::query("INSERT INTO cloud_operation_results(operation_id,state,result_digest) VALUES($1,'unknown',$2)").bind(&op).bind(proof).execute(&mut **tx).await?;
        }
        sqlx::query(
            "UPDATE cloud_operation_state SET state=$2,version=version+1 WHERE operation_id=$1",
        )
        .bind(op)
        .bind(if sent { "unknown" } else { "failed" })
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub(crate) fn model_output(value: &Value) -> Result<(), ()> {
    let c = catalog()?;
    c.tools
        .validate_output(
            &c.tools.pin(NAME).map_err(|_| ())?,
            value,
            OutputChannel::Model,
        )
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_results_use_the_same_tagged_envelope_as_the_shared_runtime() {
        let raw = json!({"path":".","snapshot_digest":"a".repeat(64),"entries":[{"name":"input.txt","kind":"file","bytes":"23"}],"next_cursor":null,"omitted_unsupported":false});
        assert!(verify_output(&raw).is_err());
        let envelope = pack_directory(raw.clone()).expect("valid E03 directory result");
        assert_eq!(envelope, json!({"kind":"list_directory","data":raw}));
        verify_output(&envelope).expect("catalog output check");
    }
}
