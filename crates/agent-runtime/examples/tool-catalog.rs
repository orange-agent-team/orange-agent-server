//! Offline descriptor export only. This cannot authorize or execute any tool.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let catalog = match arguments.as_slice() {
        [] => orange_agent_runtime::catalog::ToolCatalog::builtin(true, true)?,
        [flag] if flag == "--with-editing" => {
            orange_agent_runtime::catalog::ToolCatalog::builtin_editing(true, true)?
        }
        _ => {
            return Err(
                "Expected no arguments or --with-editing for descriptor export only".into(),
            );
        }
    };
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
