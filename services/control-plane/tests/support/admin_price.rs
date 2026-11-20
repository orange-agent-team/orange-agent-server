use orange_control_plane::admin::hash_secret;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Explicit admitted CPU and metering configuration for isolated governance tests.
pub async fn seed(pool: &PgPool, owner: &str, node: &str) -> Value {
    let profile = Uuid::new_v4().to_string();
    let execution = hash_secret(&profile);
    let model = hash_secret("fixture model");
    let executable = hash_secret("fixture executable");
    let rules = json!({"execution_digest":execution,"input_rule":"llama-chat-template-input-v1","output_rule":orange_model_runtime::metering::PROFILE,"tool_schema_rule":orange_model_runtime::tool_schema::RULE});
    let meter =
        orange_protocol::request_digest("orange.trusted-meter.v1", &rules).expect("meter digest");
    let mut descriptor = rules;
    for (key, value) in [
        ("profile_digest", json!(meter)),
        ("model_version", json!("fixture")),
        ("model_sha256", json!(model)),
        ("executable_sha256", json!(executable)),
        ("context_tokens", json!(4096)),
    ] {
        descriptor[key] = value;
    }
    sqlx::query("INSERT INTO reviewed_execution_profiles(execution_profile_id,model_version_id,model_sha256,execution_profile_digest) VALUES($1,'fixture',$2,$3)").bind(&profile).bind(model).bind(execution).execute(pool).await.expect("admit execution fixture");
    sqlx::query("INSERT INTO reviewed_cpu_profiles(execution_profile_id,executable_sha256,model_bytes,context_tokens,startup_timeout_ms) VALUES($1,$2,1024,4096,5000)").bind(&profile).bind(executable).execute(pool).await.expect("admit CPU fixture");
    sqlx::query("INSERT INTO trusted_meter_profiles(profile_digest,descriptor) VALUES($1,$2)")
        .bind(&meter)
        .bind(descriptor)
        .execute(pool)
        .await
        .expect("admit meter fixture");
    json!({"scope":{"environment":"dev","kind":"tenant","scope_id":owner},"node_id":node,"execution_profile_id":profile,"meter_profile_digest":meter,"input_price":"2000","output_price":"6000","output_limit":128,"fee_bps":120})
}
