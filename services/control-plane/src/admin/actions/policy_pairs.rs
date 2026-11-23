use super::*;

#[test]
fn accepted_actions_require_their_own_payload_family() {
    let nodes = ["node.drain", "node.quarantine", "node.resume"];
    let families: &[(&[&str], Value)] = &[
        (
            &["device_issuer.withdraw"],
            json!({"signing_key_sha256":"a".repeat(64)}),
        ),
        (
            &["admin.role_grant", "admin.role_revoke"],
            json!({"principal_id":"subject","role":"auditor"}),
        ),
        (&nodes, json!({"node_id":"node"})),
        (
            &["case.update"],
            json!({"change":"note","case_id":"case","note":"test note"}),
        ),
        (
            &["payment.refund"],
            json!({"payment_order_id":"order","amount":{"currency":"USD","minor_units":"100"},"reconciliation_case_id":"case"}),
        ),
        (
            &["pricing.publish"],
            json!({"price_version_id":"price","review_report_id":"report","effective_at":"2030-01-01T00:00:00Z"}),
        ),
        (
            &["support_access.grant"],
            json!({"subject_user_id":"subject","user_authorization_id":"consent","resource_ids":["resource"],"content_fields":["file_content"],"expires_at":"2030-01-01T00:00:00Z"}),
        ),
        (&["support_access.revoke"], json!({"grant_id":"grant"})),
    ];
    let names = families
        .iter()
        .flat_map(|(names, _)| names.iter().copied())
        .chain(["unspecified"])
        .collect::<Vec<_>>();
    for (accepted, payload) in families {
        for name in &names {
            let r:ActionRequest=serde_json::from_value(json!({"scope":{"environment":"dev","kind":"tenant","scope_id":"tenant"},
                "ticket_id":"ticket","reason":"Static policy match","target_version":"v1","action_type":name,"payload":payload})).expect("typed request");
            assert_eq!(
                r.permission().is_ok(),
                accepted.contains(name),
                "{name} and {accepted:?}"
            );
        }
    }
}
