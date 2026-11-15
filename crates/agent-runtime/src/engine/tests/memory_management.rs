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
