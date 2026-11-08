use super::*;
use crate::engine::{ModelFuture, ModelGateway, RunConfig};
use orange_protocol::model::{Message, ResponseCandidate, Role};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
struct Fixture {
    _base: tempfile::TempDir,
    private: std::path::PathBuf,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = tempfile::tempdir().expect("temp");
        let private = base.path().join("private");
        let root = base.path().join("workspace");
        std::fs::create_dir(&private).expect("private");
        std::fs::create_dir(&root).expect("workspace");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
                .expect("mode");
        }
        std::fs::write(root.join("note.txt"), "workspace data").expect("file");
        Self {
            _base: base,
            private,
            root,
        }
    }
    fn journal(&self) -> Journal {
        Journal::open(&self.private.join("state.sqlite")).expect("journal")
    }
    fn authorize(&self, journal: &mut Journal) -> WorkspaceView {
        journal
            .authorize_workspace("alice", "consent-1", &self.root, "read-only-v1")
            .expect("consent")
    }
}
fn config(id: &str, grant: &WorkspaceGrant) -> RunConfig {
    RunConfig {
        owner: "alice".into(),
        run_id: id.into(),
        policy: grant.policy().into(),
        environment: grant.environment().into(),
        model_version: "test-local-model".into(),
        max_steps: 10,
        max_invocations: 5,
        max_output_tokens: 128,
        deadline_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64
            + 300,
    }
}
fn messages() -> Vec<Message> {
    vec![Message::plain(
        Role::User,
        "private prompt not for list projection".into(),
    )]
}
#[test]
fn consent_is_idempotent_owned_and_does_not_authorize_private_state() {
    let f = Fixture::new();
    let mut j = f.journal();
    let view = f.authorize(&mut j);
    assert_eq!(f.authorize(&mut j).workspace_id, view.workspace_id);
    assert_eq!(
        j.authorize_workspace("alice", "another-consent", &f.root, "read-only-v1")
            .expect("same active object")
            .workspace_id,
        view.workspace_id
    );
    assert!(matches!(
        j.authorize_workspace("alice", "consent-1", &f.root, "different-policy"),
        Err(WorkspaceStoreError::Conflict)
    ));
    assert!(matches!(
        j.workspace("bob", &view.workspace_id),
        Err(WorkspaceStoreError::NotFound)
    ));
    assert!(j.workspaces("bob", true).expect("no leak").is_empty());
    assert!(
        j.authorize_workspace("alice", "private-root", &f.private, "read-only-v1")
            .is_err()
    );
    assert!(
        j.authorize_workspace(
            "alice",
            "parent-root",
            f.root.parent().expect("parent"),
            "read-only-v1"
        )
        .is_err()
    );
    let grant = j
        .open_workspace("alice", &view.workspace_id)
        .expect("grant");
    assert!(!grant.is_revoked());
    drop(j);
    assert!(grant.is_revoked());
    let mut reopened = f.journal();
    let fresh = reopened
        .open_workspace("alice", &view.workspace_id)
        .expect("fresh live authority");
    assert!(!fresh.is_revoked());
    assert_eq!(fresh.environment(), view.environment);
}
struct FinalModel;
impl ModelGateway for FinalModel {
    fn generate<'a>(
        &'a self,
        _: &'a orange_protocol::model::GenerateRequest,
        _: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        Box::pin(async {
            Ok(ResponseCandidate {
                response_id: "response-final".into(),
                text: "private final answer".into(),
                tool_calls: vec![],
                native_usage: None,
                finish_reason: "stop".into(),
            })
        })
    }
}
#[tokio::test]
async fn revocation_fences_all_linked_runs_and_history_is_content_free() {
    let f = Fixture::new();
    let mut j = f.journal();
    let workspace = f.authorize(&mut j);
    let one = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("one");
    let two = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("two");
    j.start_agent(&config("run-one", &one), messages(), &one)
        .expect("run1");
    j.start_agent(&config("run-two", &two), messages(), &two)
        .expect("run2");
    let (_sender, receiver) = watch::channel(false);
    j.advance_agent("alice", "run-one", &one, &FinalModel, receiver)
        .await
        .expect("complete");
    let page = j
        .list_agent_runs("alice", Some(&workspace.workspace_id), None, 1)
        .expect("page");
    assert_eq!(page.runs[0].run_id, "run-two");
    let rest = j
        .list_agent_runs("alice", None, page.next_cursor.as_deref(), 1)
        .expect("rest");
    assert_eq!(rest.runs[0].run_id, "run-one");
    assert!(rest.next_cursor.is_none());
    assert!(
        !serde_json::to_string(&page)
            .expect("json")
            .contains("private prompt")
    );
    assert!(
        !serde_json::to_string(&rest)
            .expect("json")
            .contains("private final")
    );
    assert!(
        j.list_agent_runs("bob", None, None, 100)
            .expect("no leak")
            .runs
            .is_empty()
    );
    assert!(
        j.list_agent_runs("bob", Some(&workspace.workspace_id), None, 1)
            .is_err()
    );
    assert!(j.list_agent_runs("alice", None, Some("01"), 1).is_err());
    assert!(
        j.revoke_workspace("bob", &workspace.workspace_id, "1")
            .is_err()
    );
    assert!(
        j.revoke_workspace("alice", &workspace.workspace_id, "2")
            .is_err()
    );
    assert!(!two.is_revoked());
    let revoked = j
        .revoke_workspace("alice", &workspace.workspace_id, "1")
        .expect("revoke");
    assert!(!revoked.active);
    assert_eq!(revoked.version, "2");
    assert!(one.is_revoked() && two.is_revoked());
    assert!(
        j.agent_view("alice", "run-one")
            .expect("no revival")
            .final_text
            .is_none()
    );
    assert!(
        j.list_agent_runs("alice", None, None, 100)
            .expect("runs")
            .runs
            .iter()
            .all(|r| !r.active)
    );
    assert_eq!(
        j.revoke_workspace("alice", &workspace.workspace_id, "1")
            .expect("idempotent")
            .version,
        "2"
    );
    drop(j);
    let mut j = f.journal();
    assert!(matches!(
        j.open_workspace("alice", &workspace.workspace_id),
        Err(WorkspaceStoreError::Revoked)
    ));
    assert!(matches!(
        j.authorize_workspace("alice", "consent-1", &f.root, "read-only-v1"),
        Err(WorkspaceStoreError::Revoked)
    ));
    let new = j
        .authorize_workspace("alice", "explicit-new-consent", &f.root, "read-only-v1")
        .expect("fresh consent");
    assert_ne!(new.workspace_id, workspace.workspace_id);
    assert!(
        j.agent_view("alice", "run-one")
            .expect("old run")
            .final_text
            .is_none()
    );
    assert!(
        j.connection
            .execute("DELETE FROM workspace_audit", [])
            .is_err()
    );
}
#[tokio::test]
async fn cancelling_one_run_does_not_revoke_workspace_or_other_run() {
    let f = Fixture::new();
    let mut j = f.journal();
    let workspace = f.authorize(&mut j);
    let one = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("one");
    let two = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("two");
    j.start_agent(&config("one", &one), messages(), &one)
        .expect("run1");
    j.start_agent(&config("two", &two), messages(), &two)
        .expect("run2");
    j.cancel_agent("alice", "one", &one).expect("cancel");
    assert!(one.is_revoked());
    assert!(!two.is_revoked());
    assert!(
        j.workspace("alice", &workspace.workspace_id)
            .expect("workspace")
            .active
    );
    let (_sender, receiver) = watch::channel(false);
    assert_eq!(
        j.advance_agent("alice", "two", &two, &FinalModel, receiver)
            .await
            .expect("independent")
            .phase,
        crate::engine::Phase::Completed
    );
}
#[tokio::test]
async fn revocation_signal_stops_acquired_permits_before_durable_commit() {
    let f = Fixture::new();
    let mut j = f.journal();
    let workspace = f.authorize(&mut j);
    let grant = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("grant");
    let request = crate::workspace::WorkspaceRequest::parse(
        "read_file",
        &serde_json::json!({"path":"note.txt"}),
    )
    .expect("request");
    let prepared = grant
        .prepare("alice", grant.environment(), grant.policy(), request)
        .await
        .expect("prepare");
    j.workspace_revoker("alice", &workspace.workspace_id)
        .expect("signal")
        .request_revoke();
    assert!(!prepared.is_authorized());
    assert!(matches!(
        prepared.execute().await,
        Err(WorkspaceError::Revoked)
    ));
    assert!(j.open_workspace("alice", &workspace.workspace_id).is_err());
    j.revoke_workspace("alice", &workspace.workspace_id, "1")
        .expect("durable commit");
    grant.wait_drained().await;
}
#[test]
fn replaced_directory_does_not_inherit_previous_consent() {
    let f = Fixture::new();
    let mut j = f.journal();
    let workspace = f.authorize(&mut j);
    let grant = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("grant");
    std::fs::rename(&f.root, f.root.with_file_name("old-workspace")).expect("move");
    std::fs::create_dir(&f.root).expect("new object");
    let private_area = f.private.with_file_name("another-private");
    std::fs::create_dir(&private_area).expect("private area");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&private_area, std::fs::Permissions::from_mode(0o700))
            .expect("mode");
    }
    assert!(
        matches!(
            j.reserve_private_directory(&private_area),
            Err(WorkspaceStoreError::Conflict)
        ),
        "private registration must not overlook a replaced active root"
    );
    assert!(matches!(
        j.open_workspace("alice", &workspace.workspace_id),
        Err(WorkspaceStoreError::Workspace(
            WorkspaceError::EnvironmentChanged
        ))
    ));
    assert!(grant.is_revoked());
    assert!(
        !j.workspace("alice", &workspace.workspace_id)
            .expect("old")
            .active
    );
    let new = j
        .authorize_workspace("alice", "new-object-consent", &f.root, "read-only-v1")
        .expect("new root");
    assert_ne!(workspace.resource_identity, new.resource_identity);
    assert_ne!(workspace.workspace_id, new.workspace_id);
}
struct NeverModel;
impl ModelGateway for NeverModel {
    fn generate<'a>(
        &'a self,
        _: &'a orange_protocol::model::GenerateRequest,
        _: watch::Receiver<bool>,
    ) -> ModelFuture<'a> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
async fn unknown_execution_cannot_be_bypassed_by_revoking_and_regranting_same_resource() {
    let f = Fixture::new();
    let mut j = f.journal();
    let workspace = f.authorize(&mut j);
    let grant = j
        .open_workspace("alice", &workspace.workspace_id)
        .expect("grant");
    j.start_agent(&config("unknown-run", &grant), messages(), &grant)
        .expect("run");
    let (_sender, receiver) = watch::channel(false);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(15),
            j.advance_agent("alice", "unknown-run", &grant, &NeverModel, receiver)
        )
        .await
        .is_err()
    );
    j.revoke_workspace("alice", &workspace.workspace_id, "1")
        .expect("revoke");
    assert!(matches!(
        j.authorize_workspace("alice", "new-consent", &f.root, "read-only-v1"),
        Err(WorkspaceStoreError::RecoveryRequired)
    ));
    drop(j);
    let mut j = f.journal();
    assert!(matches!(
        j.authorize_workspace("alice", "another-consent", &f.root, "read-only-v1"),
        Err(WorkspaceStoreError::RecoveryRequired)
    ));
    let moved = f.root.with_file_name("renamed-workspace");
    std::fs::rename(&f.root, &moved).expect("rename same object");
    assert!(
        matches!(
            j.authorize_workspace("alice", "renamed-consent", &moved, "read-only-v1"),
            Err(WorkspaceStoreError::RecoveryRequired)
        ),
        "a new path must not hide the same unresolved resource"
    );
    assert!(
        !j.list_agent_runs("alice", None, None, 100)
            .expect("status")
            .runs[0]
            .active
    );
}
#[test]
fn schema_two_upgrades_without_inventing_workspace_permissions() {
    let f = Fixture::new();
    let mut j = f.journal();
    let grant = WorkspaceGrant::open(
        "alice",
        "legacy-grant",
        "read-only-v1",
        &f.root,
        std::slice::from_ref(&f.private),
    )
    .expect("legacy");
    j.start_agent(&config("legacy-run", &grant), messages(), &grant)
        .expect("run");
    j.connection.execute_batch("DROP TABLE editing_publication_steps; DROP TABLE editing_publications; DROP TABLE editing_approvals; DROP TABLE editing_previews; DROP TABLE editing_consent_audit; DROP TABLE editing_request_keys; DROP TABLE editing_consents; DROP TABLE runtime_tool_approvals; DROP TABLE runtime_tool_catalogs; DROP TABLE runtime_catalog_legacy_approvals; DROP TABLE runtime_catalog_legacy; DROP TABLE runtime_creation_capabilities; DROP TABLE creation_consent_audit; DROP TABLE creation_request_keys; DROP TABLE creation_consents; DROP TABLE runtime_questions; DROP TABLE runtime_capabilities; DROP TABLE workspace_audit; DROP TABLE run_workspaces; DROP TABLE workspace_request_keys; DROP TABLE local_workspaces; PRAGMA user_version=2;").expect("v2 fixture without later-version tables");
    drop(j);
    let j = f.journal();
    assert!(
        j.workspaces("alice", true)
            .expect("no invented grants")
            .is_empty()
    );
    let runs = j
        .list_agent_runs("alice", None, None, 100)
        .expect("history");
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(runs.runs[0].run_id, "legacy-run");
    assert!(runs.runs[0].workspace_id.is_none());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_data_volume_alias_cannot_include_private_host_state() {
    let f = Fixture::new();
    let mut j = f.journal();
    let ancestor = f
        .private
        .parent()
        .expect("parent")
        .canonicalize()
        .expect("canonical");
    let alias = std::path::Path::new("/System/Volumes/Data")
        .join(ancestor.strip_prefix("/").expect("absolute"));
    assert!(
        alias.is_dir(),
        "macOS Data-volume alias required for this target-specific test"
    );
    assert!(matches!(
        j.authorize_workspace("alice", "alias-consent", &alias, "read-only-v1"),
        Err(WorkspaceStoreError::Workspace(WorkspaceError::Forbidden))
    ));
}

#[test]
fn private_ipc_area_cannot_overlap_active_consent_or_be_granted_later() {
    let f = Fixture::new();
    let mut j = f.journal();
    let workspace = f.authorize(&mut j);
    let nested = f.root.join("private-model");
    let safe = f.private.with_file_name("private-model");
    for path in [&nested, &safe] {
        std::fs::create_dir(path).expect("private area");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("mode");
        }
    }
    assert!(matches!(
        j.reserve_private_directory(&nested),
        Err(WorkspaceStoreError::Conflict)
    ));
    assert!(
        j.workspace("alice", &workspace.workspace_id)
            .expect("unchanged consent")
            .active
    );
    j.reserve_private_directory(&safe)
        .expect("reserve outside workspaces");
    j.reserve_private_directory(&safe).expect("idempotent");
    assert!(matches!(
        j.authorize_workspace("alice", "private-ipc-consent", &safe, "read-only-v1"),
        Err(WorkspaceStoreError::Workspace(WorkspaceError::Forbidden))
    ));
    assert!(j.open_workspace("alice", &workspace.workspace_id).is_ok());
}
