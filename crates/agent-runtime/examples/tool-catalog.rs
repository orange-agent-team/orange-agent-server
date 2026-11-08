//! Offline descriptor export only. This cannot authorize or execute any tool.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let catalog = orange_agent_runtime::catalog::ToolCatalog::builtin(true, true)?;
    let pins: std::collections::BTreeMap<_, _> = catalog
        .document()
        .tools
        .iter()
        .map(|spec| Ok((spec.tool_version.clone(), spec.execution_digest()?)))
        .collect::<Result<_, orange_protocol::tool_spec::SpecError>>()?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"rule":"tool-spec-execution.v1","pins":pins})
        )?
    );
    Ok(())
}
