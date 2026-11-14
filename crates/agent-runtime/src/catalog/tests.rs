use super::*;
use crate::{
    storage::private_temp,
    workspace::{WorkspaceGrant, WorkspaceRequest},
};
use serde_json::json;

#[test]
fn catalog_pins_are_order_independent_but_not_schema_or_policy_independent() {
    let catalog = ToolCatalog::builtin(true, true).expect("builtins");
    let mut document = catalog.document();
    document.tools.reverse();
    let reordered = ToolCatalog::new(document.tools.clone()).expect("canonical execution order");
    assert_eq!(reordered.revision(), catalog.revision());
    assert_ne!(
        reordered.document().document_digest,
        catalog.document().document_digest
    );
    assert!(matches!(
        ToolCatalog::restore(document.clone()),
        Err(CatalogError::SnapshotChanged)
    ));
    ToolCatalog::restore(reordered.document()).expect("matching document digest");
    let mut wording = catalog.document();
    wording.tools[0].description = "changed help text".into();
    assert!(matches!(
        ToolCatalog::restore(wording),
        Err(CatalogError::SnapshotChanged)
    ));
    let pin = catalog.pin("write_file").expect("pin");
    let mut modified = document.clone();
    modified
        .tools
        .iter_mut()
        .find(|t| t.name == "write_file")
        .expect("write")
        .output_schema = json!({"type":"string"});
    assert!(matches!(
        ToolCatalog::restore(modified.clone()),
        Err(CatalogError::PinChanged)
    ));
    let new = ToolCatalog::new(modified.tools).expect("valid different spec");
    assert!(matches!(
        new.validate_input(&pin, &json!({"path":"a","content":"b"})),
        Err(CatalogError::PinChanged)
    ));
    let duplicate = document.tools[0].clone();
    document.tools.push(duplicate);
    assert!(matches!(
        ToolCatalog::new(document.tools),
        Err(CatalogError::Duplicate)
    ));
}
#[test]
fn absent_and_unknown_tools_or_schema_authority_are_not_admitted() {
    let read = ToolCatalog::builtin(false, false).expect("read catalog");
    assert_eq!(read.definitions().expect("model descriptions").len(), 4);
    assert!(matches!(
        read.pin("write_file"),
        Err(CatalogError::Unavailable)
    ));
    assert!(matches!(
        read.pin("ask_user"),
        Err(CatalogError::Unavailable)
    ));
    let full = ToolCatalog::builtin(true, true).expect("full catalog");
    assert_eq!(full.definitions().expect("model descriptions").len(), 6);
    let mut document = full.document();
    document.tools[0].output_schema = json!({"$ref":"https://example.invalid/schema"});
    assert!(ToolCatalog::new(document.tools).is_err());
    let pin = full.pin("read_file").expect("pin");
    for bad in [
        json!({"path":"a","grant_access":true}),
        json!({"path":"a","max_bytes":null}),
        json!({"path":"a","max_bytes":0}),
    ] {
        assert!(full.validate_input(&pin, &bad).is_err());
    }
    let mut pin = pin;
    pin.tool_version = "read_file.v2".into();
    assert!(matches!(
        full.validate_input(&pin, &json!({"path":"a"})),
        Err(CatalogError::PinChanged)
    ));
}
#[tokio::test]
async fn actual_filesystem_outputs_match_their_contract_and_catalog_does_not_grant_write() {
    let private = private_temp();
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("proof.txt"), "catalog proof").expect("fixture");
    let grant = WorkspaceGrant::open(
        "alice",
        "grant",
        "policy",
        root.path(),
        &[private.path().into()],
    )
    .expect("read grant");
    let catalog = ToolCatalog::builtin(true, true).expect("catalog");
    for (name, args) in [
        ("read_file", json!({"path":"proof.txt"})),
        ("list_directory", json!({"path":"."})),
        ("search_files", json!({"path":".","glob":"proof.txt"})),
        ("search_text", json!({"path":".","query":"catalog"})),
    ] {
        let pin = catalog.pin(name).expect("pin");
        catalog.validate_input(&pin, &args).expect("input");
        let request = WorkspaceRequest::parse(name, &args).expect("semantic parser");
        assert_eq!(pin.tool_version, request.version());
        let receipt = grant
            .execute("alice", grant.environment(), "policy", request)
            .await
            .expect("real filesystem");
        let raw = serde_json::to_value(receipt.result()).expect("result");
        catalog
            .validate_output(&pin, &raw, OutputChannel::Durable)
            .expect("actual durable output");
        let model: Value = serde_json::from_str(receipt.model_text()).expect("model output");
        catalog
            .validate_output(&pin, &model, OutputChannel::Model)
            .expect("actual model output");
        let mut leaked = raw;
        leaked["grant_access"] = json!(true);
        assert!(
            catalog
                .validate_output(&pin, &leaked, OutputChannel::Durable)
                .is_err()
        );
    }
    let args = json!({"path":"never.txt","content":"metadata cannot write"});
    let pin = catalog.pin("write_file").expect("visible descriptor");
    catalog
        .validate_input(&pin, &args)
        .expect("well-formed proposal");
    assert!(
        grant
            .prepare(
                "alice",
                grant.environment(),
                "policy",
                WorkspaceRequest::parse("write_file", &args).expect("request")
            )
            .await
            .is_err()
    );
    assert!(!root.path().join("never.txt").exists());
}
#[test]
fn output_contract_rejects_unsafe_decimal_and_unknown_fields_and_enforces_projection_bound() {
    let catalog = ToolCatalog::builtin(true, true).expect("catalog");
    let pin = catalog.pin("write_file").expect("write pin");
    let valid = json!({"kind":"write_file","data":{"path":"a","sha256":"a".repeat(64),"bytes":"9007199254740993"}});
    catalog
        .validate_output(&pin, &valid, OutputChannel::Model)
        .expect("exact decimal string");
    for bytes in [json!(9007199254740993u64), json!("01"), json!("-1")] {
        let mut bad = valid.clone();
        bad["data"]["bytes"] = bytes;
        assert!(
            catalog
                .validate_output(&pin, &bad, OutputChannel::Model)
                .is_err()
        );
    }
    catalog
        .validate_output(
            &pin,
            &json!({"error":"create_rejected"}),
            OutputChannel::Model,
        )
        .expect("sanitized failure");
    assert!(
        catalog
            .validate_output(
                &pin,
                &json!({"error":"create_rejected","details":"unreviewed data"}),
                OutputChannel::Model
            )
            .is_err()
    );
    let pin = catalog.pin("read_file").expect("read pin");
    let value = json!({"kind":"read_file","data":{"path":"a","sha256":"a".repeat(64),"total_bytes":"70000","start_byte":"0","end_byte":"70000","content":"x".repeat(70000),"truncated":false,"next_byte":null}});
    catalog
        .validate_output(&pin, &value, OutputChannel::Durable)
        .expect("raw result within declared limit");
    assert!(matches!(
        catalog.validate_output(&pin, &value, OutputChannel::Model),
        Err(CatalogError::Limit)
    ));
    let pin = catalog.pin("ask_user").expect("questions");
    catalog
        .validate_output(&pin, &json!({"answers":["yes"]}), OutputChannel::Model)
        .expect("answer context");
    assert!(
        !catalog
            .describe("ask_user")
            .expect("descriptor")
            .needs_approval()
    );
    assert!(
        catalog
            .describe("write_file")
            .expect("descriptor")
            .needs_approval()
    );
    assert!(
        !catalog
            .describe("write_file")
            .expect("descriptor")
            .parallel_safe()
    );
}

#[test]
fn versioned_builtin_execution_pins_and_legacy_model_order_are_fixed() {
    let expected: Value = serde_json::from_str(include_str!(
        "../../../../contracts/fixtures/builtin-tool-pins.v1.json"
    ))
    .expect("versioned contract fixture");
    let catalog = ToolCatalog::builtin(true, true).expect("catalog");
    let actual: std::collections::BTreeMap<_, _> = catalog
        .document()
        .tools
        .iter()
        .map(|s| (s.tool_version.clone(), s.execution_digest().expect("pin")))
        .collect();
    assert_eq!(
        serde_json::to_value(actual).expect("pins"),
        expected["pins"],
        "Do not silently change an existing tool version; introduce a new version and explicit compatibility policy"
    );
    let mut legacy = crate::workspace::definitions();
    legacy.push(orange_protocol::questions::definition());
    legacy.push(crate::workspace::creation::definition());
    assert_eq!(
        serde_json::to_value(catalog.definitions().expect("definitions")).expect("catalog"),
        serde_json::to_value(legacy).expect("original"),
        "Catalog integration must not accidentally reorder the fixed model tool prompt"
    );
}

#[test]
fn explicit_edit_catalog_adds_one_fixed_write_contract_without_changing_legacy_pins() {
    let expected: Value = serde_json::from_str(include_str!(
        "../../../../contracts/fixtures/editing-tool-pins.v1.json"
    ))
    .expect("editing contract fixture");
    let catalog = ToolCatalog::builtin_editing(true, true).expect("editing catalog");
    let actual: std::collections::BTreeMap<_, _> = catalog
        .document()
        .tools
        .iter()
        .map(|s| (s.tool_version.clone(), s.execution_digest().expect("pin")))
        .collect();
    assert_eq!(
        serde_json::to_value(actual).expect("pins"),
        expected["pins"]
    );
    let legacy = ToolCatalog::builtin(true, true).expect("legacy catalog");
    for definition in legacy.definitions().expect("legacy order") {
        assert_eq!(
            legacy.pin(&definition.name).expect("old pin"),
            catalog.pin(&definition.name).expect("unchanged pin")
        );
    }
    let names = catalog
        .definitions()
        .expect("definitions")
        .into_iter()
        .map(|d| d.name)
        .collect::<Vec<_>>();
    assert_eq!(names.last().map(String::as_str), Some("apply_patch"));
    let spec = catalog.describe("apply_patch").expect("edit contract");
    assert!(spec.needs_approval());
    assert_eq!(spec.effect_class, "local_write");
    assert_eq!(spec.concurrency, "exclusive");
    assert!(legacy.pin("apply_patch").is_err());
}
