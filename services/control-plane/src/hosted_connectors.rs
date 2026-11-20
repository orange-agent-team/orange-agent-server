//! Fixed tool definitions and effect pins used on both sides of the platform connector approval boundary.
use orange_agent_runtime::catalog::{OutputChannel, ToolCatalog};
use orange_protocol::{
    model::{ToolCall, ToolDefinition},
    tool_spec::{ToolLimits, ToolSpec},
};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub(crate) const TOOLS: [(&str, &str, &str); 6] = [
    (
        "postgres",
        "list_views",
        "List the views permitted by the PostgreSQL grant selected by the user.",
    ),
    (
        "postgres",
        "describe_view",
        "Show the approved columns and paging key of one registered PostgreSQL view.",
    ),
    (
        "postgres",
        "query_view",
        "Read a bounded page from a registered PostgreSQL view using only authorized columns.",
    ),
    (
        "s3",
        "list_objects",
        "List keys in the S3 bucket prefix authorized by the user.",
    ),
    (
        "s3",
        "head_object",
        "Inspect the size, ETag and version of an object relative to the authorized S3 prefix.",
    ),
    (
        "s3",
        "read_object",
        "Create a version-bound attachment from an inspected object in the authorized S3 prefix.",
    ),
];
pub(crate) fn family(name: &str) -> Option<&'static str> {
    TOOLS
        .iter()
        .find(|(_, n, _)| *n == name)
        .map(|(f, _, _)| *f)
}
fn expanded(v: &Value, defs: &Value, depth: u8) -> Result<Value, ()> {
    if depth > 20 {
        return Err(());
    }
    if let Some(r) = v.get("$ref") {
        let name = r
            .as_str()
            .and_then(|s| s.strip_prefix("#/$defs/"))
            .ok_or(())?;
        if v.as_object().ok_or(())?.len() != 1 {
            return Err(());
        }
        return expanded(defs.get(name).ok_or(())?, defs, depth + 1);
    }
    match v {
        Value::Array(items) => items
            .iter()
            .map(|item| expanded(item, defs, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(fields) => fields
            .iter()
            .map(|(k, x)| Ok((k.clone(), expanded(x, defs, depth + 1)?)))
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(Value::Object),
        primitive => Ok(primitive.clone()),
    }
}
fn catalog() -> Result<&'static ToolCatalog, ()> {
    static CATALOG: OnceLock<Result<ToolCatalog, ()>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let pg: Value = serde_json::from_str(include_str!(
                "../../../contracts/json-schema/pg-connector.schema.json"
            ))
            .map_err(|_| ())?;
            let s3: Value = serde_json::from_str(include_str!(
                "../../../contracts/json-schema/s3-connector.schema.json"
            ))
            .map_err(|_| ())?;
            let mut specs = Vec::new();
            for (family, name, description) in TOOLS {
                let schema = if family == "postgres" { &pg } else { &s3 };
                let defs = &schema["$defs"];
                let variant = defs["Action"]["oneOf"]
                    .as_array()
                    .ok_or(())?
                    .iter()
                    .find(|v| v["properties"]["action"]["const"] == name)
                    .ok_or(())?;
                let mut args = expanded(variant, defs, 0)?;
                args["properties"]
                    .as_object_mut()
                    .ok_or(())?
                    .remove("action");
                args["required"]
                    .as_array_mut()
                    .ok_or(())?
                    .retain(|v| v != "action");
                let mut output = defs["Output"].clone();
                let payload = match (family, name) {
                    ("postgres", name) => {
                        let marker = match name {
                            "list_views" => "view_ids",
                            "describe_view" => "paging_key",
                            "query_view" => "rows",
                            _ => return Err(()),
                        };
                        defs["Output"]["properties"]["data"]["oneOf"]
                            .as_array()
                            .ok_or(())?
                            .iter()
                            .find(|v| v["properties"].get(marker).is_some())
                            .ok_or(())?
                    }
                    ("s3", "list_objects") => &defs["List"],
                    ("s3", "head_object") => &defs["Head"],
                    ("s3", "read_object") => &defs["Artifact"],
                    _ => return Err(()),
                };
                output["properties"]["data"] = payload.clone();
                specs.push(ToolSpec {
                    spec_version: 1,
                    name: name.into(),
                    tool_version: format!("{family}.{name}.v1"),
                    description: description.into(),
                    effect_class: "read_only".into(),
                    concurrency: "exclusive".into(),
                    approval_required: true,
                    execution_environment: "platform_connector".into(),
                    input_schema: args,
                    output_schema: expanded(&output, defs, 0)?,
                    limits: ToolLimits {
                        execution_timeout_ms: Some(30000),
                        max_argument_bytes: 16384,
                        max_result_bytes: 2 * 1024 * 1024,
                        max_model_result_bytes: 16384,
                    },
                });
            }
            ToolCatalog::new(specs).map_err(|_| ())
        })
        .as_ref()
        .map_err(|_| ())
}
pub(crate) fn definition(name: &str) -> Result<ToolDefinition, ()> {
    catalog()?
        .describe(name)
        .ok_or(())?
        .model_definition()
        .map_err(|_| ())
}
pub(crate) fn digest(name: &str) -> Result<String, ()> {
    Ok(catalog()?.pin(name).map_err(|_| ())?.execution_digest)
}
pub(crate) fn public(name: &str) -> Result<Value, ()> {
    let c = catalog()?;
    let spec = c.describe(name).ok_or(())?;
    Ok(
        json!({"name":name,"connector":family(name).ok_or(())?,"tool_version":spec.tool_version,"execution_digest":digest(name)?,
        "execution_environment":spec.execution_environment,"approval_required":true,"model_definition":definition(name)?}),
    )
}
pub(crate) fn action(call: &ToolCall, expected: &str, pin: &str) -> Result<Value, ()> {
    if call.name != expected || !orange_protocol::valid_id(&call.id) {
        return Err(());
    }
    let c = catalog()?;
    let actual = c.pin(expected).map_err(|_| ())?;
    if actual.execution_digest != pin {
        return Err(());
    }
    c.validate_input(&actual, &call.arguments).map_err(|_| ())?;
    let mut body = call.arguments.clone();
    body.as_object_mut()
        .ok_or(())?
        .insert("action".into(), json!(expected));
    Ok(body)
}
pub(crate) fn check_result(name: &str, result: &Value) -> Result<(), ()> {
    let c = catalog()?;
    c.validate_output(
        &c.pin(name).map_err(|_| ())?,
        result,
        OutputChannel::Durable,
    )
    .map_err(|_| ())
}

/// A durable terminal disposition makes workspace cleanup independent of the reviewing browser.
pub(crate) async fn reconcile(pool: &sqlx::PgPool) -> Result<bool, sqlx::Error> {
    use sqlx::Row;
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT r.run_id,receipt.state FROM cloud_runs r JOIN cloud_connector_decisions d USING(run_id)
        LEFT JOIN cloud_connector_effect_receipts receipt USING(run_id)
        WHERE r.status='waiting_approval' AND (receipt.run_id IS NOT NULL OR d.expires_at<=clock_timestamp())
        ORDER BY d.expires_at LIMIT 1 FOR UPDATE OF r SKIP LOCKED").fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        let purged=sqlx::query("DELETE FROM cloud_connector_result_content WHERE run_id IN (SELECT run_id FROM cloud_connector_result_content WHERE retain_until<=clock_timestamp() ORDER BY retain_until LIMIT 64 FOR UPDATE SKIP LOCKED)").execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(purged.rows_affected() > 0);
    };
    let run: String = row.try_get("run_id")?;
    let state = row.try_get::<Option<String>, _>("state")?;
    if state.is_none() {
        sqlx::query("INSERT INTO cloud_connector_effect_receipts(run_id,state) VALUES($1,'unknown') ON CONFLICT DO NOTHING")
            .bind(&run).execute(&mut *tx).await?;
    }
    let state = state.as_deref().unwrap_or("unknown");
    let (status, event) = match state {
        "succeeded" => ("paused", "connector_result_committed"),
        "rejected" => ("failed", "connector_operation_rejected"),
        "failed" => ("failed", "connector_operation_failed"),
        "unknown" => ("recovery_required", "connector_operation_unknown"),
        _ => {
            return Err(sqlx::Error::Protocol(
                "Invalid connector disposition".into(),
            ));
        }
    };
    let version: i64 = sqlx::query_scalar(
        "UPDATE cloud_runs SET status=$2,version=version+1 WHERE run_id=$1 RETURNING version",
    )
    .bind(&run)
    .bind(status)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE cloud_workspaces SET status='draining',lease_until=NULL,epoch=epoch+1,version=version+1 WHERE run_id=$1 AND status='ready'").bind(&run).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO cloud_run_events(run_id,run_version,kind) VALUES($1,$2,$3)")
        .bind(run)
        .bind(version)
        .bind(event)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn six_connector_definitions_can_only_materialize_the_pinned_action() {
        for (_, name, _) in TOOLS {
            let def = definition(name).expect("definition");
            assert_eq!(def.name, name);
            assert!(def.parameters["properties"].get("action").is_none());
            assert_eq!(public(name).expect("descriptor")["approval_required"], true);
        }
        let mut call = ToolCall {
            id: "call_1".into(),
            name: "list_views".into(),
            arguments: json!({}),
        };
        let pin = digest("list_views").expect("pin");
        assert_eq!(
            action(&call, "list_views", &pin).expect("validated"),
            json!({"action":"list_views"})
        );
        call.arguments = json!({"source":"other"});
        assert!(action(&call, "list_views", &pin).is_err());
        call.arguments = json!({});
        assert!(action(&call, "list_views", &digest("list_objects").expect("other")).is_err());
        check_result(
            "list_views",
            &json!({"kind":"external_untrusted_data","data":{"view_ids":["one"]}}),
        )
        .expect("result");
    }
    #[test]
    fn model_input_and_user_result_must_match_one_selected_operation() {
        use crate::{cloud_model::Selection, cloud_tools::Choice};
        use orange_protocol::model::{GenerateRequest, Message, Role};
        for (_, name, _) in TOOLS {
            let mut selected = Selection {
                model_version: "model-a".into(),
                price_version: "price-a".into(),
                invocation_id: "inv-a".into(),
                output_limit: 64,
                tool: Some(Choice {
                    name: name.into(),
                    execution_digest: digest(name).expect("pin"),
                }),
                continue_after_tool: false,
            };
            assert!(selected.valid());
            let mut req = GenerateRequest {
                model_version: selected.model_version.clone(),
                invocation_id: selected.invocation_id.clone(),
                max_output_tokens: 64,
                messages: vec![Message::plain(
                    Role::User,
                    "request a source operation".into(),
                )],
                tools: vec![definition(name).expect("definition")],
            };
            assert!(
                selected
                    .projection(&serde_json::to_vec(&req).expect("body"))
                    .is_ok()
            );
            req.tools[0].description = "changed provider prompt".into();
            assert!(
                selected
                    .projection(&serde_json::to_vec(&req).expect("body"))
                    .is_err()
            );
            selected.continue_after_tool = true;
            assert!(!selected.valid());
        }
        let listing =
            json!({"kind":"external_untrusted_data","data":{"keys":["a.txt"],"next_after":null}});
        assert!(check_result("list_objects", &listing).is_ok());
        assert!(check_result("list_views", &listing).is_err());
        assert!(check_result("read_object", &listing).is_err());
    }
}
