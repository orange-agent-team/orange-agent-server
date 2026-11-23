use orange_protocol::marketplace::JobInputRef;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    wire: Value,
    accepted: bool,
}

#[test]
fn typed_artifact_references_round_trip_and_refuse_invalid_wire_values() {
    let cases: Vec<Case> = serde_json::from_str(include_str!(
        "../../../contracts/fixtures/job-input-interop.v1.json"
    ))
    .expect("v1 interop fixture");
    let mut seen = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    for case in cases {
        assert!(seen.insert(case.name.clone()), "duplicate fixture name");
        let parsed = serde_json::from_value::<JobInputRef>(case.wire.clone())
            .ok()
            .filter(|value| value.pin().is_ok());
        assert_eq!(parsed.is_some(), case.accepted, "{}", case.name);
        if let Some(value) = parsed {
            kinds.insert(case.wire["kind"].as_str().expect("tag").to_owned());
            assert_eq!(serde_json::to_value(value).expect("encode"), case.wire);
        }
    }
    assert_eq!(
        kinds,
        ["embedding", "image", "text", "transcription"]
            .map(String::from)
            .into()
    );
}
