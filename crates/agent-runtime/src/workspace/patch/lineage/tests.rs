use super::*;
use crate::workspace::patch::authority::EditFileGrant;
use crate::workspace::patch::review::tests::Fixture;
use crate::{Operation, ResponseAuthority};
use orange_domain::ExecutionIdentity;
use orange_protocol::model::ToolCall;
use std::fs;

fn time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}
fn response(j: &Journal, run: &str, inv: &str, call: &str, name: &str, args: serde_json::Value) {
    j.register_run("alice", run, "policy").expect("run");
    let body = ResponseCandidate {
        response_id: format!("response-{inv}"),
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: call.into(),
            name: name.into(),
            arguments: args,
        }],
        native_usage: None,
        finish_reason: "tool_calls".into(),
    };
    let execution = ExecutionIdentity {
        connection_epoch: 1,
        fencing: 1,
        attempt: 1,
    };
    j.commit_response(
        "alice",
        run,
        inv,
        ResponseAuthority {
            current: execution,
            incoming: execution,
            complete: true,
        },
        &request_digest("model-response-v1", &body).expect("response digest"),
    )
    .expect("explicit trusted adapter response fixture");
    j.connection
        .execute(
            "UPDATE responses SET body=? WHERE invocation=?",
            params![serde_json::to_string(&body).expect("body"), inv],
        )
        .expect("persist complete response fixture");
}
pub(in crate::workspace::patch) async fn source(
    j: &mut Journal,
    f: &Fixture,
    run: &str,
    id: &str,
    request: WorkspaceRequest,
) -> serde_json::Value {
    let inv = format!("inv-{id}");
    let call = format!("call-{id}");
    let mut encoded = serde_json::to_value(&request).expect("request");
    let args = encoded.get_mut("arguments").expect("arguments");
    args.as_object_mut()
        .expect("object")
        .retain(|_, v| !v.is_null());
    response(j, run, &inv, &call, request.name(), args.clone());
    let digest = request.digest().expect("arguments digest");
    let op = Operation {
        id,
        owner: "alice",
        run,
        invocation: &inv,
        argument_digest: &digest,
        tool_version: request.version(),
        environment: f.read.environment(),
        policy_version: "policy",
        requires_approval: true,
    };
    j.prepare(&op).expect("intent");
    j.connection
        .execute(
            "UPDATE operations SET request_json=?,call_id=? WHERE operation_id=?",
            params![serde_json::to_string(&request).expect("request"), call, id],
        )
        .expect("bind exact request");
    j.approve(&op, "alice", time() + 300)
        .expect("exact fixture approval");
    j.claim(&op, time()).expect("dispatch barrier");
    let actual = f
        .read
        .execute("alice", f.read.environment(), "policy", request)
        .await
        .expect("actual authorized readonly tool");
    let value = serde_json::to_value(actual.result()).expect("result");
    let tx = j.connection.transaction().expect("receipt transaction");
    tx.execute("UPDATE operations SET state='succeeded',result_json=?,result_digest=?,model_result=? WHERE operation_id=? AND state='dispatching'",params![serde_json::to_string(actual.result()).expect("result"),request_digest("tool-result-v1",actual.result()).expect("result digest"),actual.model_text(),id]).expect("actual receipt");
    tx.execute(
        "INSERT INTO events(run_id,operation_id,kind) VALUES(?,?,'operation.succeeded')",
        params![run, id],
    )
    .expect("ordered event");
    tx.commit().expect("source receipt durable before patch");
    value
}
async fn patch(
    j: &mut Journal,
    f: &Fixture,
    edit: &EditFileGrant,
    run: &str,
    id: &str,
    old: &str,
    new: &str,
) -> PublishedPatch {
    let args = PatchArgs {
        path: "file.txt".into(),
        expected_sha256: hex::encode(Sha256::digest(format!("{old}\n"))),
        patch: format!("@@ -1 +1 @@\n-{old}\n+{new}\n"),
    };
    patch_args(j, f, edit, run, id, args)
        .await
        .expect("real publication")
}
async fn patch_args(
    j: &mut Journal,
    f: &Fixture,
    edit: &EditFileGrant,
    run: &str,
    id: &str,
    args: PatchArgs,
) -> Result<PublishedPatch, PatchError> {
    let inv = format!("inv-{id}");
    let call = format!("call-{id}");
    response(
        j,
        run,
        &inv,
        &call,
        "apply_patch",
        serde_json::to_value(&args).expect("args"),
    );
    let digest = args.digest().expect("digest");
    let operation = Operation {
        id,
        owner: "alice",
        run,
        invocation: &inv,
        argument_digest: &digest,
        tool_version: "apply_patch.v1",
        environment: edit.environment(),
        policy_version: "policy",
        requires_approval: true,
    };
    let preview = j
        .prepare_patch_review(edit, &f.read, &operation, args, &call)
        .await
        .expect("review");
    j.approve_patch_review(
        edit,
        &f.read,
        &operation,
        &preview.preview_digest,
        "alice",
        time() + 300,
    )
    .await
    .expect("exact patch approval");
    let token = j
        .begin_patch_publication(edit, &f.read, &operation)
        .await
        .expect("dispatch");
    j.publish_patch_dispatch(edit, &f.read, token)
}
fn read() -> WorkspaceRequest {
    WorkspaceRequest::parse("read_file", &serde_json::json!({"path":"file.txt"})).expect("request")
}
async fn validate(j: &Journal, f: &Fixture, run: &str, id: &str) -> serde_json::Value {
    serde_json::to_value(
        j.revalidate_workspace_history("alice", run, id, &f.read)
            .await
            .expect("history validated")
            .result(),
    )
    .expect("JSON")
}
#[tokio::test]
async fn multiple_versions_preserve_each_source_boundary_and_survive_journal_reopen() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    let first = source(&mut j, &f, "run", "first", read()).await;
    patch(&mut j, &f, &edit, "run", "edit1", "old", "middle").await;
    let middle = source(&mut j, &f, "run", "middle", read()).await;
    let search = WorkspaceRequest::parse(
        "search_files",
        &serde_json::json!({"path":".","glob":"**/*"}),
    )
    .expect("search");
    let middle_search = source(&mut j, &f, "run", "middle-search", search).await;
    patch(&mut j, &f, &edit, "run", "edit2", "middle", "latest").await;
    assert_eq!(validate(&j, &f, "run", "first").await, first);
    assert_eq!(validate(&j, &f, "run", "middle").await, middle);
    assert_eq!(
        validate(&j, &f, "run", "middle-search").await,
        middle_search
    );
    let edges: i64 = j
        .connection
        .query_row("SELECT count(*) FROM patch_version_edges", [], |r| r.get(0))
        .expect("edges");
    assert_eq!(edges, 2);
    assert!(
        j.connection
            .execute("UPDATE patch_version_edges SET body='{}'", [])
            .is_err()
    );
    assert!(
        j.connection
            .execute("DELETE FROM patch_version_edges", [])
            .is_err()
    );
    drop(j);
    let j = f.journal();
    assert_eq!(validate(&j, &f, "run", "first").await, first);
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("no replay"),
        b"latest\n"
    );
}
#[tokio::test]
async fn current_target_deleted_replaced_or_changed_is_never_hidden_by_old_backup() {
    for case in ["deleted", "replaced", "changed"] {
        let f = Fixture::new();
        let mut j = f.journal();
        let edit = f.edit(&mut j);
        source(&mut j, &f, "run", "read", read()).await;
        patch(&mut j, &f, &edit, "run", "edit", "old", "new").await;
        let path = f.root.path().join("file.txt");
        if case == "deleted" {
            fs::remove_file(&path).expect("owned deletion");
        } else if case == "replaced" {
            fs::rename(&path, f.root.path().join("external-saved")).expect("retain prior inode");
            fs::write(&path, b"new\n").expect("same bytes new inode");
        } else {
            fs::write(&path, b"external bytes\n").expect("external edit");
        }
        assert!(
            j.revalidate_workspace_history("alice", "run", "read", &f.read)
                .await
                .is_err(),
            "case {case}"
        );
    }
}
#[tokio::test]
async fn late_backup_write_and_permission_revocation_invalidate_history() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    source(&mut j, &f, "run", "read", read()).await;
    let receipt = patch(&mut j, &f, &edit, "run", "edit", "old", "new").await;
    fs::write(
        f.root.path().join(receipt.retained_original),
        b"late writer\n",
    )
    .expect("backup mutation");
    assert!(
        j.revalidate_workspace_history("alice", "run", "read", &f.read)
            .await
            .is_err()
    );
    f.read.request_revoke();
    assert!(
        j.revalidate_workspace_history("alice", "run", "read", &f.read)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn another_run_or_owner_cannot_lend_its_edit_history() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    source(&mut j, &f, "reader-run", "read", read()).await;
    patch(&mut j, &f, &edit, "writer-run", "edit", "old", "new").await;
    assert!(
        j.revalidate_workspace_history("alice", "reader-run", "read", &f.read)
            .await
            .is_err()
    );
    assert!(
        j.revalidate_workspace_history("bob", "reader-run", "read", &f.read)
            .await
            .is_err()
    );
    assert!(
        j.revalidate_workspace_history("alice", "writer-run", "read", &f.read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn query_scope_unicode_paths_and_directory_cursor_are_preserved() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    fs::create_dir(f.root.path().join("资料")).expect("folder");
    fs::write(f.root.path().join("资料/a.txt"), b"old\n").expect("source");
    fs::write(f.root.path().join("资料/b.txt"), b"other\n").expect("other source");
    let first = source(
        &mut j,
        &f,
        "run",
        "list-first",
        WorkspaceRequest::parse(
            "list_directory",
            &serde_json::json!({"path":"资料","limit":1}),
        )
        .expect("list"),
    )
    .await;
    let cursor = first["data"]["next_cursor"].as_str().expect("cursor");
    let next = source(
        &mut j,
        &f,
        "run",
        "list-next",
        WorkspaceRequest::parse(
            "list_directory",
            &serde_json::json!({"path":"资料","limit":1,"cursor":cursor}),
        )
        .expect("next page"),
    )
    .await;
    let text = source(
        &mut j,
        &f,
        "run",
        "text",
        WorkspaceRequest::parse(
            "search_text",
            &serde_json::json!({"path":"资料","query":"old","glob":"*.txt"}),
        )
        .expect("search text"),
    )
    .await;
    patch_args(
        &mut j,
        &f,
        &edit,
        "run",
        "edit",
        PatchArgs {
            path: "资料/a.txt".into(),
            expected_sha256: hex::encode(Sha256::digest(b"old\n")),
            patch: "@@ -1 +1 @@\n-old\n+new and longer\n".into(),
        },
    )
    .await
    .expect("publish");
    assert_eq!(validate(&j, &f, "run", "list-first").await, first);
    assert_eq!(validate(&j, &f, "run", "list-next").await, next);
    assert_eq!(validate(&j, &f, "run", "text").await, text);
}

#[tokio::test]
async fn arbitrary_protected_names_are_not_suppressed_and_physical_scan_is_bounded() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    fs::write(
        f.root.path().join(".orange-agent-tmp-unrelated"),
        b"owned unrelated file",
    )
    .expect("existing protected fixture");
    let req = WorkspaceRequest::parse(
        "search_files",
        &serde_json::json!({"path":".","glob":"**/*"}),
    )
    .expect("query");
    let before = source(&mut j, &f, "run", "search", req).await;
    assert_eq!(before["data"]["scanned_entries"], 2);
    patch(&mut j, &f, &edit, "run", "edit", "old", "new").await;
    assert_eq!(validate(&j, &f, "run", "search").await, before);
    fs::write(
        f.root.path().join(".orange-agent-tmp-not-in-journal"),
        b"external fixture",
    )
    .expect("extra protected name");
    assert!(
        j.revalidate_workspace_history("alice", "run", "search", &f.read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn source_deletion_between_historical_read_and_return_is_not_exposed() {
    for remove_backup in [false, true] {
        let f = Fixture::new();
        let mut j = f.journal();
        let edit = f.edit(&mut j);
        source(&mut j, &f, "run", "read", read()).await;
        let receipt = patch(&mut j, &f, &edit, "run", "edit", "old", "new").await;
        let path = f.root.path().join(if remove_backup {
            receipt.retained_original
        } else {
            "file.txt".into()
        });
        assert!(
            j.revalidate_history_with_hook("alice", "run", "read", &f.read, move || {
                fs::remove_file(path).expect("owned concurrent deletion");
            })
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn failing_version_edge_transaction_leaves_effect_unknown_without_lineage() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    source(&mut j, &f, "run", "read", read()).await;
    j.connection.execute_batch("CREATE TRIGGER edge_fault BEFORE INSERT ON patch_version_edges BEGIN SELECT RAISE(ABORT,'owned edge fault');END;").expect("failure after receipt and event writes");
    let result = patch_args(
        &mut j,
        &f,
        &edit,
        "run",
        "edit",
        PatchArgs {
            path: "file.txt".into(),
            expected_sha256: hex::encode(Sha256::digest(b"old\n")),
            patch: "@@ -1 +1 @@\n-old\n+new\n".into(),
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        j.state("alice", "edit").expect("state").as_deref(),
        Some("unknown")
    );
    let count: i64 = j
        .connection
        .query_row("SELECT count(*) FROM patch_version_edges", [], |r| r.get(0))
        .expect("no edge");
    assert_eq!(count, 0);
    let raw: Option<String> = j
        .connection
        .query_row(
            "SELECT result_json FROM operations WHERE operation_id='edit'",
            [],
            |r| r.get(0),
        )
        .expect("receipt rolled back");
    assert!(raw.is_none());
    assert_eq!(
        fs::read(f.root.path().join("file.txt")).expect("effect preserved"),
        b"new\n"
    );
    assert!(
        j.revalidate_workspace_history("alice", "run", "read", &f.read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn in_place_archival_preserves_source_validation_without_granting_write_access() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    let before = source(&mut j, &f, "run", "read", read()).await;
    patch(&mut j, &f, &edit, "run", "edit", "old", "new").await;
    let review = j
        .prepare_patch_retention("alice", "edit", &f.read, "archive")
        .await
        .expect("archive preview");
    j.approve_patch_retention("alice", &f.read, &review.review_id, &review.digest, "alice")
        .await
        .expect("retain in place");
    assert_eq!(validate(&j, &f, "run", "read").await, before);
    let receipt = j
        .revalidate_workspace_history("alice", "run", "read", &f.read)
        .await
        .expect("read lease");
    f.read.request_revoke();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), f.read.wait_drained())
            .await
            .is_err()
    );
    drop(receipt);
    tokio::time::timeout(std::time::Duration::from_secs(1), f.read.wait_drained())
        .await
        .expect("drained");
    assert!(
        j.revalidate_workspace_history("alice", "run", "read", &f.read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn historical_scan_allowance_only_covers_verified_new_retention_entries() {
    let f = Fixture::new();
    let mut j = f.journal();
    let edit = f.edit(&mut j);
    for n in 0..9998 {
        fs::write(
            f.root
                .path()
                .join(format!(".orange-agent-tmp-owned-fixture-{n}")),
            b"",
        )
        .expect("bounded owned directory fixture");
    }
    let request = WorkspaceRequest::parse(
        "search_files",
        &serde_json::json!({"path":".","glob":"**/*"}),
    )
    .expect("search");
    let before = source(&mut j, &f, "run", "search", request).await;
    assert_eq!(before["data"]["scanned_entries"], 9999);
    assert_eq!(before["data"]["truncated"], false);
    patch(&mut j, &f, &edit, "run", "first-edit", "old", "middle").await;
    patch(&mut j, &f, &edit, "run", "second-edit", "middle", "latest").await;
    assert_eq!(
        fs::read_dir(f.root.path()).expect("physical names").count(),
        10001
    );
    assert_eq!(validate(&j, &f, "run", "search").await, before);
    fs::write(
        f.root.path().join(".orange-agent-tmp-unapproved-extra"),
        b"",
    )
    .expect("unregistered extra entry");
    assert!(
        j.revalidate_workspace_history("alice", "run", "search", &f.read)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn changed_source_response_or_missing_version_evidence_is_not_adopted() {
    for case in [
        "response",
        "missing-edge",
        "changed-receipt",
        "cancelled-run",
    ] {
        let f = Fixture::new();
        let mut j = f.journal();
        let edit = f.edit(&mut j);
        source(&mut j, &f, "run", "read", read()).await;
        patch(&mut j, &f, &edit, "run", "edit", "old", "new").await;
        match case {
            "response" => {
                j.connection
                    .execute(
                        "UPDATE responses SET body='{}' WHERE invocation='inv-read'",
                        [],
                    )
                    .expect("owned corrupt response fixture");
            }
            "missing-edge" => {
                j.connection
                    .execute_batch(
                        "DROP TRIGGER patch_edge_no_delete; DELETE FROM patch_version_edges;",
                    )
                    .expect("owned missing evidence fixture");
            }
            "changed-receipt" => {
                j.connection
                    .execute(
                        "UPDATE operations SET result_json='{}' WHERE operation_id='edit'",
                        [],
                    )
                    .expect("owned damaged receipt fixture");
            }
            "cancelled-run" => {
                j.connection
                    .execute("UPDATE runs SET active=0 WHERE run_id='run'", [])
                    .expect("revoked run");
            }
            _ => unreachable!(),
        }
        assert!(
            j.revalidate_workspace_history("alice", "run", "read", &f.read)
                .await
                .is_err(),
            "{case}"
        );
    }
}
