use super::*;
use crate::memory::{MemoryGrant, MemoryMutation, MemoryOrigin, MemoryPayload, MemoryWriteRequest};
fn setup(j: &mut Journal, owner: &str, scope: &str) -> MemoryGrant {
    j.create_memory_scope(owner, scope, false).expect("scope");
    j.register_run(owner, &format!("{owner}-seed"), "policy-v1")
        .expect("user source");
    j.authorize_memory(owner, scope, "policy-v1", true)
        .expect("grant")
}
fn put(j: &mut Journal, g: &MemoryGrant, id: &str, enabled: bool) {
    let p = j
        .prepare_memory_write(
            g,
            id,
            MemoryWriteRequest {
                entry_id: id.into(),
                expected_version: None,
                source_run: format!("{}-seed", g.actor()),
                origin: MemoryOrigin::UserDeclaration,
                mutation: MemoryMutation::Put,
                payload: Some(MemoryPayload {
                    title: format!("Title {id}"),
                    content: "甲乙 user text".into(),
                    enabled,
                }),
            },
        )
        .expect("draft");
    let hash = p.view(j).expect("view").digest.clone();
    j.approve_memory_write(p, &hash).expect("approve");
}
fn select(id: &str, disabled: bool) -> MemoryReadSelection {
    MemoryReadSelection {
        entry_id: id.into(),
        expected_version: None,
        start_byte: 0,
        max_bytes: 4096,
        include_disabled: disabled,
    }
}
#[tokio::test]
async fn management_reads_without_fabricated_workspace_and_binds_pagination_to_scope() {
    let f = Fixture::new();
    let mut j = f.open();
    let a = setup(&mut j, "alice", "a");
    let b = setup(&mut j, "bob", "b");
    put(&mut j, &a, "a", true);
    put(&mut j, &a, "b", true);
    put(&mut j, &a, "disabled", false);
    let auth = MemoryManagementAuthority::new(&a);
    let first = j
        .list_memory_verified(auth, None, 1, false)
        .await
        .expect("page");
    let view = first.view(&j).expect("view");
    assert_eq!(view.entries[0].entry_id, "a");
    let cursor = view.next_cursor.clone().expect("cursor");
    assert_ne!(cursor, "a");
    assert!(
        j.list_memory_verified(MemoryManagementAuthority::new(&b), Some(&cursor), 1, false)
            .await
            .is_err()
    );
    assert!(
        j.list_memory_verified(auth, Some(&cursor), 1, true)
            .await
            .is_err()
    );
    let second = j
        .list_memory_verified(auth, Some(&cursor), 1, false)
        .await
        .expect("next");
    assert_eq!(second.view(&j).expect("view").entries[0].entry_id, "b");
    assert!(second.view(&j).expect("view").next_cursor.is_none());
    assert!(
        j.read_memory_verified(auth, select("disabled", false))
            .await
            .is_err()
    );
    let disabled = j
        .read_memory_verified(auth, select("disabled", true))
        .await
        .expect("explicit management view");
    assert!(!disabled.view(&j).expect("view").enabled);
    let sliced = j
        .read_memory_verified(
            auth,
            MemoryReadSelection {
                entry_id: "a".into(),
                expected_version: Some("1".into()),
                start_byte: 0,
                max_bytes: 4,
                include_disabled: false,
            },
        )
        .await
        .expect("UTF8");
    assert_eq!(sliced.view(&j).expect("view").content, "甲");
    assert_eq!(
        sliced.view(&j).expect("view").next_byte.as_deref(),
        Some("3")
    );
    a.request_revoke();
    assert!(first.view(&j).is_err());
    assert!(disabled.view(&j).is_err());
    drop(first);
    drop(second);
    drop(disabled);
    drop(sliced);
    tokio::time::timeout(Duration::from_secs(1), a.wait_drained())
        .await
        .expect("management snapshots drained");
}
#[tokio::test]
async fn file_derived_management_data_requires_current_source_permission_and_hides_filtered_titles()
{
    let mut f = Fixture::new();
    let mut j = f.open();
    let ws = j
        .authorize_workspace("alice", "source", f.workspace.path(), "policy-v1")
        .expect("consent");
    f.grant = j.open_workspace("alice", &ws.workspace_id).expect("grant");
    f.config.environment = f.grant.environment().into();
    let grant = setup(&mut j, "alice", "personal");
    let grants = vec![grant.clone()];
    let id = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    let auth = MemoryManagementAuthority::new(&grant);
    let hidden = j
        .list_memory_verified(auth, None, 10, false)
        .await
        .expect("bounded partial result");
    let value = hidden.view(&j).expect("view");
    assert!(value.entries.is_empty());
    assert!(value.omitted_unavailable);
    assert!(!serde_json::to_string(value).expect("wire").contains(&id));
    assert!(
        j.read_memory_verified(auth, select(&id, false))
            .await
            .is_err()
    );
    drop(hidden);
    let sources = vec![f.grant.clone()];
    let auth = auth.with_source_workspaces(&sources);
    let record = j
        .read_memory_verified(auth, select(&id, false))
        .await
        .expect("actual verified source");
    assert_eq!(record.view(&j).expect("view").content, "allowed data");
    let page = j
        .list_memory_verified(auth, None, 10, false)
        .await
        .expect("authorized page");
    assert_eq!(page.view(&j).expect("view").entries.len(), 1);
    j.revoke_workspace("alice", &ws.workspace_id, &ws.version)
        .expect("persistent withdrawal");
    assert!(record.view(&j).is_err());
    assert!(page.view(&j).is_err());
    drop(record);
    drop(page);
    assert!(
        j.read_memory_verified(auth, select(&id, false))
            .await
            .is_err()
    );
    let page = j
        .list_memory_verified(auth, None, 10, false)
        .await
        .expect("no content reintroduced");
    assert!(page.view(&j).expect("view").entries.is_empty());
    assert!(page.view(&j).expect("view").omitted_unavailable);
}
#[tokio::test]
async fn sparse_unproven_records_have_bounded_scans_without_ids_in_the_cursor() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = setup(&mut j, "alice", "scope");
    put(&mut j, &grant, "z-visible", true);
    // Deliberately orphaned records model an incomplete import. They must not be
    // presented as verified memory, or leak titles while requesting more pages.
    let payload = MemoryPayload {
        title: "SECRET_ORPHAN_TITLE".into(),
        content: "SECRET_ORPHAN_BODY".into(),
        enabled: true,
    };
    let body = serde_json::to_string(&payload).expect("body");
    let sha = orange_protocol::request_digest("memory-payload.v1", &payload).expect("digest");
    for n in 0..201 {
        j.connection.execute("INSERT INTO memory_entries VALUES('scope',?,1,'active',?,?,'alice','alice-seed','model_inference')",params![format!("blocked-{n:03}"),body,sha]).expect("owned import fault");
    }
    let auth = MemoryManagementAuthority::new(&grant);
    let first = j
        .list_memory_verified(auth, None, 1, false)
        .await
        .expect("bounded scan");
    let v = first.view(&j).expect("view");
    assert!(v.entries.is_empty());
    assert!(v.scan_limited && v.omitted_unavailable);
    let encoded = serde_json::to_string(v).expect("wire");
    assert!(!encoded.contains("blocked-") && !encoded.contains("SECRET_ORPHAN"));
    let cursor = v.next_cursor.clone().expect("opaque continuation");
    drop(first);
    let next = j
        .list_memory_verified(auth, Some(&cursor), 1, false)
        .await
        .expect("resume after scanned entries");
    assert_eq!(
        next.view(&j).expect("view").entries[0].entry_id,
        "z-visible"
    );
    assert!(next.view(&j).expect("view").next_cursor.is_none());
    drop(next);
    drop(j);
    let j = f.open();
    let fresh = j
        .authorize_memory("alice", "scope", "policy-v1", true)
        .expect("fresh scope");
    assert!(
        j.list_memory_verified(
            MemoryManagementAuthority::new(&fresh),
            Some(&cursor),
            1,
            false
        )
        .await
        .is_err(),
        "opaque cursor cannot cross a Journal session"
    );
}

#[tokio::test]
async fn human_edit_rechecks_prior_sources_and_cannot_approve_with_payload_only_hash() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = setup(&mut j, "alice", "personal");
    let grants = vec![grant.clone()];
    let id = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    j.register_run("alice", "human-edit", "policy-v1")
        .expect("real human activity");
    let request = MemoryWriteRequest {
        entry_id: id.clone(),
        expected_version: Some("1".into()),
        source_run: "human-edit".into(),
        origin: MemoryOrigin::UserDeclaration,
        mutation: MemoryMutation::Put,
        payload: Some(MemoryPayload {
            title: "Corrected by user".into(),
            content: "Explicit user declaration".into(),
            enabled: true,
        }),
    };
    let no_sources = MemoryManagementAuthority::new(&grant);
    assert!(
        j.prepare_user_memory_write(no_sources, "edit", request.clone())
            .await
            .is_err()
    );
    let sources = vec![f.grant.clone()];
    let auth = no_sources.with_source_workspaces(&sources);
    let preview = j
        .prepare_user_memory_write(auth, "edit", request)
        .await
        .expect("source verified review");
    assert_eq!(
        preview.prior.as_ref().expect("prior").content,
        "allowed data"
    );
    assert_ne!(preview.review.digest, preview.approval_digest);
    tokio::time::timeout(Duration::from_secs(1), grant.wait_drained())
        .await
        .expect("no receipt lease retained while human considers preview");
    assert!(
        j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.review.digest)
            .await
            .is_err()
    );
    std::fs::write(f.workspace.path().join("note.txt"), "external replacement")
        .expect("source changed");
    assert!(
        j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
            .await
            .is_err()
    );
    assert_eq!(
        j.user_memory_write_by_key("alice", "personal", "edit")
            .expect("metadata")
            .expect("draft")
            .state,
        "pending"
    );
    std::fs::write(f.workspace.path().join("note.txt"), "allowed data")
        .expect("restore own source");
    let outcome = j
        .approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
        .await
        .expect("exact human approval");
    assert_eq!(outcome.result_version.as_deref(), Some("2"));
    assert!(
        j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
            .await
            .is_err()
    );
    grant.request_revoke();
    let recovered = j
        .user_memory_write_metadata("alice", "personal", &outcome.draft_id)
        .expect("metadata without body grant")
        .expect("committed");
    assert_eq!(recovered, outcome);
    assert!(
        j.user_memory_write_metadata("bob", "personal", &outcome.draft_id)
            .expect("foreign absent")
            .is_none()
    );
    assert!(
        j.read_memory_verified(auth, select(&id, false))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn human_edit_late_source_acl_change_rolls_back_fact_and_delete_is_explicit() {
    let mut f = Fixture::new();
    let mut j = f.open();
    let workspace = j
        .authorize_workspace("alice", "source", f.workspace.path(), "policy-v1")
        .expect("root");
    f.grant = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("read");
    f.config.environment = f.grant.environment().into();
    let grant = setup(&mut j, "alice", "personal");
    let grants = vec![grant.clone()];
    let id = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    j.register_run("alice", "human-delete", "policy-v1")
        .expect("human source");
    let sources = vec![f.grant.clone()];
    let auth = MemoryManagementAuthority::new(&grant).with_source_workspaces(&sources);
    let request = MemoryWriteRequest {
        entry_id: id.clone(),
        expected_version: Some("1".into()),
        source_run: "human-delete".into(),
        origin: MemoryOrigin::UserDeclaration,
        mutation: MemoryMutation::Delete,
        payload: None,
    };
    let preview = j
        .prepare_user_memory_write(auth, "delete", request.clone())
        .await
        .expect("review");
    j.connection.execute_batch("CREATE TRIGGER human_source_loss AFTER UPDATE ON memory_entries BEGIN UPDATE local_workspaces SET active=0; END;").expect("explicit commit race fixture");
    assert!(
        j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
            .await
            .is_err()
    );
    assert_eq!(
        j.read_memory_verified(auth, select(&id, false))
            .await
            .expect("original remains")
            .view(&j)
            .expect("view")
            .version,
        "1"
    );
    assert_eq!(
        j.user_memory_write_by_key("alice", "personal", "delete")
            .expect("metadata")
            .expect("draft")
            .state,
        "pending"
    );
    j.connection
        .execute_batch("DROP TRIGGER human_source_loss;")
        .expect("remove race");
    j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
        .await
        .expect("explicit delete");
    assert!(
        j.read_memory_verified(auth, select(&id, false))
            .await
            .is_err()
    );
    let mut forged = request;
    forged.origin = MemoryOrigin::ModelInference;
    assert!(
        j.prepare_user_memory_write(auth, "forged", forged)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn confirmed_human_edit_keeps_known_source_dependencies_after_commit() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = setup(&mut j, "alice", "personal");
    let grants = vec![grant.clone()];
    let id = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    j.register_run("alice", "human-confirmation", "policy-v1")
        .expect("human operation");
    let sources = vec![f.grant.clone()];
    let auth = MemoryManagementAuthority::new(&grant).with_source_workspaces(&sources);
    let preview = j
        .prepare_user_memory_write(
            auth,
            "confirm",
            MemoryWriteRequest {
                entry_id: id.clone(),
                expected_version: Some("1".into()),
                source_run: "human-confirmation".into(),
                origin: MemoryOrigin::UserDeclaration,
                mutation: MemoryMutation::Put,
                payload: Some(MemoryPayload {
                    title: "Confirmed note".into(),
                    content: "allowed data, confirmed by user".into(),
                    enabled: true,
                }),
            },
        )
        .await
        .expect("verified prior source");
    j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
        .await
        .expect("exact human confirmation");
    assert!(
        j.read_memory_verified(MemoryManagementAuthority::new(&grant), select(&id, false))
            .await
            .is_err(),
        "human confirmation cannot erase a known file dependency"
    );
    assert!(
        j.read_memory(&grant, &id, Some("2"), 0, 4096, false)
            .is_err(),
        "raw Store cannot bypass inherited sources"
    );
    j.read_memory_verified(auth, select(&id, false))
        .await
        .expect("original source still authorized");
    f.grant.request_revoke();
    assert!(
        j.read_memory_verified(auth, select(&id, false))
            .await
            .is_err(),
        "withdrawn source cannot reappear as an independent user declaration"
    );
}

#[tokio::test]
async fn long_human_confirmation_suffix_preserves_model_source_without_exhausting_graph_depth() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = setup(&mut j, "alice", "personal");
    let grants = vec![grant.clone()];
    let derived = super::memory_provenance::learn_file(&mut j, &f.config, &f.grant, &grants).await;
    put(&mut j, &grant, "independent", true);
    j.register_run("alice", "human-batch", "policy-v1")
        .expect("owned user action batch");
    let sources = vec![f.grant.clone()];
    let verified = MemoryManagementAuthority::new(&grant).with_source_workspaces(&sources);
    let plain = MemoryManagementAuthority::new(&grant);
    for (id, auth) in [(&derived, verified), (&"independent".to_owned(), plain)] {
        for n in 1..=40 {
            let preview = j
                .prepare_user_memory_write(
                    auth,
                    &format!("{id}-update-{n}"),
                    MemoryWriteRequest {
                        entry_id: id.clone(),
                        expected_version: Some(n.to_string()),
                        source_run: "human-batch".into(),
                        origin: MemoryOrigin::UserDeclaration,
                        mutation: MemoryMutation::Put,
                        payload: Some(MemoryPayload {
                            title: "Human confirmation".into(),
                            content: format!("Confirmed version {}", n + 1),
                            enabled: true,
                        }),
                    },
                )
                .await
                .expect("bounded edit preview");
            j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
                .await
                .expect("exact update");
        }
    }
    assert_eq!(
        j.read_memory_verified(verified, select(&derived, false))
            .await
            .expect("collapsed inheritance")
            .view(&j)
            .expect("view")
            .version,
        "41"
    );
    assert!(
        j.read_memory_verified(plain, select(&derived, false))
            .await
            .is_err()
    );
    assert!(
        j.read_memory(&grant, &derived, None, 0, 4096, false)
            .is_err()
    );
    assert_eq!(
        j.read_memory(&grant, "independent", Some("41"), 0, 4096, false)
            .expect("source-free independent declarations")
            .view(&j)
            .expect("view")
            .content,
        "Confirmed version 41"
    );
    f.grant.request_revoke();
    assert!(
        j.read_memory_verified(verified, select(&derived, false))
            .await
            .is_err()
    );
    j.read_memory_verified(plain, select("independent", false))
        .await
        .expect("unrelated declaration remains available");
}
#[tokio::test]
async fn missing_human_history_is_not_treated_as_an_independent_declaration() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = setup(&mut j, "alice", "personal");
    put(&mut j, &grant, "note", true);
    let auth = MemoryManagementAuthority::new(&grant);
    for n in 1..=2 {
        let preview = j
            .prepare_user_memory_write(
                auth,
                &format!("update-{n}"),
                MemoryWriteRequest {
                    entry_id: "note".into(),
                    expected_version: Some(n.to_string()),
                    source_run: "alice-seed".into(),
                    origin: MemoryOrigin::UserDeclaration,
                    mutation: MemoryMutation::Put,
                    payload: Some(MemoryPayload {
                        title: "Note".into(),
                        content: "Current user body".into(),
                        enabled: true,
                    }),
                },
            )
            .await
            .expect("preview");
        j.approve_user_memory_write(auth, &preview.review.draft_id, &preview.approval_digest)
            .await
            .expect("approve");
    }
    j.connection.execute_batch("DROP TRIGGER memory_draft_no_delete; DELETE FROM memory_drafts WHERE scope_id='personal' AND entry_id='note' AND result_version=2;").expect("owned history corruption fixture");
    assert!(
        j.read_memory_verified(auth, select("note", false))
            .await
            .is_err()
    );
    assert!(j.read_memory(&grant, "note", None, 0, 4096, false).is_err());
}
#[test]
fn unknown_human_continuity_policy_refuses_open_without_rewriting() {
    let f = Fixture::new();
    let j = f.open();
    j.connection.execute_batch("DROP TABLE runtime_memory_continuity_policy; CREATE TABLE runtime_memory_continuity_policy(singleton INTEGER PRIMARY KEY,policy TEXT); INSERT INTO runtime_memory_continuity_policy VALUES(1,'unknown-policy');").expect("explicit future-policy fixture");
    drop(j);
    let path = f.private.path().join("journal.sqlite");
    let before = std::fs::read(&path).expect("bytes");
    assert!(matches!(
        Journal::open(&path),
        Err(crate::JournalError::UnsupportedVersion)
    ));
    assert_eq!(before, std::fs::read(path).expect("unchanged bytes"));
}

#[tokio::test]
async fn pending_user_drafts_are_recoverable_and_discardable_after_restart() {
    let f = Fixture::new();
    let mut j = f.open();
    let grant = setup(&mut j, "alice", "personal");
    let auth = MemoryManagementAuthority::new(&grant);
    let mut ids = vec![];
    for id in ["one", "two"] {
        let review = j
            .prepare_user_memory_write(
                auth,
                id,
                MemoryWriteRequest {
                    entry_id: id.into(),
                    expected_version: None,
                    source_run: "alice-seed".into(),
                    origin: MemoryOrigin::UserDeclaration,
                    mutation: MemoryMutation::Put,
                    payload: Some(MemoryPayload {
                        title: "Private".into(),
                        content: "Pending draft body".into(),
                        enabled: true,
                    }),
                },
            )
            .await
            .expect("draft");
        ids.push(review.review.draft_id);
    }
    ids.sort();
    drop(j);
    let j = f.open();
    let first = j
        .pending_user_memory_drafts("alice", "personal", None, 1)
        .expect("first owned metadata")
        .remove(0);
    assert_eq!(first.draft_id, ids[0]);
    let next = j
        .pending_user_memory_drafts("alice", "personal", Some(&first.draft_id), 10)
        .expect("second page");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].draft_id, ids[1]);
    assert!(
        j.pending_user_memory_drafts("bob", "personal", None, 10)
            .expect("filtered")
            .is_empty()
    );
    assert!(
        j.discard_user_memory_draft("alice", "personal", &first.draft_id, "incorrect")
            .is_err()
    );
    assert!(
        j.discard_user_memory_draft("bob", "personal", &first.draft_id, &first.digest)
            .is_err()
    );
    j.connection.execute_batch("CREATE TRIGGER fail_discard BEFORE INSERT ON memory_audit WHEN NEW.action='user_draft_discarded' BEGIN SELECT RAISE(ABORT,'owned fault'); END;").expect("inject");
    assert!(
        j.discard_user_memory_draft("alice", "personal", &first.draft_id, &first.digest)
            .is_err()
    );
    assert_eq!(
        j.user_memory_write_metadata("alice", "personal", &first.draft_id)
            .expect("metadata")
            .expect("receipt")
            .state,
        "pending"
    );
    j.connection
        .execute_batch("DROP TRIGGER fail_discard")
        .expect("remove");
    for _ in 0..2 {
        assert_eq!(
            j.discard_user_memory_draft("alice", "personal", &first.draft_id, &first.digest)
                .expect("exact discard")
                .state,
            "invalidated"
        );
    }
    assert_eq!(
        j.connection
            .query_row(
                "SELECT count(*) FROM memory_audit WHERE action='user_draft_discarded'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("one audit"),
        1
    );
    assert!(
        j.connection
            .query_row(
                "SELECT payload FROM memory_drafts WHERE draft_id=?",
                [first.draft_id],
                |r| r.get::<_, Option<String>>(0)
            )
            .expect("cleared")
            .is_none()
    );
    assert_eq!(
        j.pending_user_memory_drafts("alice", "personal", None, 10)
            .expect("remaining"),
        next
    );
}
