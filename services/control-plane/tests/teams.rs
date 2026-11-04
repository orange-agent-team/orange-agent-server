//! Real database role/ACL tests. User identity records are explicit fixtures, not OIDC certification.
use orange_control_plane::{
    store::Store,
    team::{TeamError, TeamStore},
};
use orange_protocol::team::*;
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    team: TeamStore,
    pool: PgPool,
    owner: String,
    org: String,
}
async fn new_user(pool: &PgPool) -> String {
    let id = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await.expect("transaction");
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("owner fixture");
    sqlx::query("INSERT INTO wallets(owner_id) VALUES($1)")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .expect("zero wallet fixture");
    sqlx::query("INSERT INTO users(user_id,issuer,subject,personal_owner_id) VALUES($1,'https://identity-fixture.invalid',$1,$1)").bind(&id).execute(&mut *tx).await.expect("identity fixture");
    tx.commit().await.expect("commit");
    id
}
async fn fixture(limit: u16) -> Fixture {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_DATABASE_URL").expect("actual PostgreSQL required"),
    )
    .await
    .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = store.pool().clone();
    let team = TeamStore::new(pool.clone());
    let owner = new_user(&pool).await;
    let org = team
        .create_organization(
            &owner,
            "create-org",
            &CreateOrganization {
                name: "Local ACL fixture".into(),
                member_limit: limit,
            },
        )
        .await
        .expect("organization")
        .organization_id;
    Fixture {
        team,
        pool,
        owner,
        org,
    }
}
async fn version(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT policy_version FROM organizations WHERE organization_id=$1")
        .bind(&f.org)
        .fetch_one(&f.pool)
        .await
        .expect("version")
}
async fn add(f: &Fixture, user: &str, role: TeamRole) -> MembershipChange {
    f.team
        .set_membership(
            &f.owner,
            &f.org,
            &Uuid::new_v4().to_string(),
            &ChangeMembership {
                user_id: user.into(),
                role: Some(role),
                expected_policy_version: version(f).await,
            },
        )
        .await
        .expect("add member")
}
#[tokio::test]
async fn five_roles_require_explicit_resource_grants_and_cannot_read_other_tenants() {
    let f = fixture(20).await;
    let mut people = Vec::new();
    for role in [
        TeamRole::Admin,
        TeamRole::Member,
        TeamRole::Billing,
        TeamRole::Viewer,
    ] {
        let user = new_user(&f.pool).await;
        add(&f, &user, role).await;
        people.push(user);
    }
    let creator = &people[1];
    let mut resource = f
        .team
        .register_resource(creator, &f.org, "resource", "artifact")
        .await
        .expect("private resource");
    assert!(
        f.team
            .resource(
                creator,
                &f.org,
                &resource.resource_id,
                ResourceAccess::Write
            )
            .await
            .is_ok()
    );
    for user in [&f.owner, &people[0], &people[2], &people[3]] {
        assert!(matches!(
            f.team
                .resource(user, &f.org, &resource.resource_id, ResourceAccess::Read)
                .await,
            Err(TeamError::Forbidden)
        ));
    }
    let illicit = ChangeResourceAcl {
        user_id: f.owner.clone(),
        access: Some(ResourceAccess::Write),
        expected_acl_version: resource.acl_version,
    };
    assert!(matches!(
        f.team
            .set_acl(
                &f.owner,
                &f.org,
                &resource.resource_id,
                "self-grant",
                &illicit
            )
            .await,
        Err(TeamError::Forbidden)
    ));
    for user in [&f.owner, &people[0], &people[2], &people[3]] {
        resource = f
            .team
            .set_acl(
                creator,
                &f.org,
                &resource.resource_id,
                &Uuid::new_v4().to_string(),
                &ChangeResourceAcl {
                    user_id: user.clone(),
                    access: Some(ResourceAccess::Write),
                    expected_acl_version: resource.acl_version,
                },
            )
            .await
            .expect("explicit creator share");
    }
    assert!(
        f.team
            .resource(
                &f.owner,
                &f.org,
                &resource.resource_id,
                ResourceAccess::Read
            )
            .await
            .is_ok()
    );
    assert!(
        f.team
            .resource(
                &people[0],
                &f.org,
                &resource.resource_id,
                ResourceAccess::Write
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        f.team
            .resource(
                &people[2],
                &f.org,
                &resource.resource_id,
                ResourceAccess::Read
            )
            .await,
        Err(TeamError::Forbidden)
    ));
    assert!(
        f.team
            .resource(
                &people[3],
                &f.org,
                &resource.resource_id,
                ResourceAccess::Read
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        f.team
            .resource(
                &people[3],
                &f.org,
                &resource.resource_id,
                ResourceAccess::Write
            )
            .await,
        Err(TeamError::Forbidden)
    ));
    let other = f
        .team
        .create_organization(
            &f.owner,
            "other-org",
            &CreateOrganization {
                name: "Other".into(),
                member_limit: 20,
            },
        )
        .await
        .expect("other organization");
    assert!(matches!(
        f.team
            .resource(
                &f.owner,
                &other.organization_id,
                &resource.resource_id,
                ResourceAccess::Read
            )
            .await,
        Err(TeamError::NotFound)
    ));
    sqlx::query("UPDATE team_resources SET deleted=TRUE WHERE resource_id=$1")
        .bind(&resource.resource_id)
        .execute(&f.pool)
        .await
        .expect("tombstone fixture");
    assert!(matches!(
        f.team
            .register_resource(creator, &f.org, "resource", "artifact")
            .await,
        Err(TeamError::NotFound)
    ));
    assert!(matches!(
        f.team
            .resource(creator, &f.org, &resource.resource_id, ResourceAccess::Read)
            .await,
        Err(TeamError::NotFound)
    ));
    assert!(
        f.team
            .set_acl(
                creator,
                &f.org,
                &resource.resource_id,
                "revive",
                &ChangeResourceAcl {
                    user_id: creator.clone(),
                    access: Some(ResourceAccess::Write),
                    expected_acl_version: resource.acl_version
                }
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn concurrent_owner_removal_cannot_remove_last_owner_and_replay_checks_current_authority() {
    let f = fixture(20).await;
    let second = new_user(&f.pool).await;
    add(&f, &second, TeamRole::Owner).await;
    let version = version(&f).await;
    let a = ChangeMembership {
        user_id: second.clone(),
        role: None,
        expected_policy_version: version,
    };
    let b = ChangeMembership {
        user_id: f.owner.clone(),
        role: None,
        expected_policy_version: version,
    };
    let (first, next) = tokio::join!(
        f.team.set_membership(&f.owner, &f.org, "remove-a", &a),
        f.team.set_membership(&second, &f.org, "remove-b", &b)
    );
    assert_ne!(first.is_ok(), next.is_ok());
    let surviving: String = sqlx::query_scalar(
        "SELECT user_id FROM memberships WHERE organization_id=$1 AND role='owner' AND active",
    )
    .bind(&f.org)
    .fetch_one(&f.pool)
    .await
    .expect("surviving owner");
    let current = if let Ok(result) = first {
        result.policy_version
    } else {
        next.expect("one winner").policy_version
    };
    assert!(matches!(
        f.team
            .set_membership(
                &surviving,
                &f.org,
                "last-owner",
                &ChangeMembership {
                    user_id: surviving.clone(),
                    role: None,
                    expected_policy_version: current
                }
            )
            .await,
        Err(TeamError::LastOwner)
    ));
    let removed = if surviving == f.owner {
        &second
    } else {
        &f.owner
    };
    let (key, request) = if removed == &f.owner {
        ("remove-a", a)
    } else {
        ("remove-b", b)
    };
    assert!(matches!(
        f.team.set_membership(removed, &f.org, key, &request).await,
        Err(TeamError::Forbidden)
    ));
    let wallet: i64 =
        sqlx::query_scalar("SELECT gift+earned+purchased FROM wallets WHERE owner_id=$1")
            .bind(&f.org)
            .fetch_one(&f.pool)
            .await
            .expect("wallet");
    assert_eq!(wallet, 0);
}
#[tokio::test]
async fn rejoining_does_not_restore_content_acl_and_admin_cannot_promote_or_exceed_capacity() {
    let f = fixture(3).await;
    let admin = new_user(&f.pool).await;
    let reader = new_user(&f.pool).await;
    add(&f, &admin, TeamRole::Admin).await;
    add(&f, &reader, TeamRole::Viewer).await;
    let resource = f
        .team
        .register_resource(&f.owner, &f.org, "private", "memory")
        .await
        .expect("resource");
    f.team
        .set_acl(
            &f.owner,
            &f.org,
            &resource.resource_id,
            "share",
            &ChangeResourceAcl {
                user_id: reader.clone(),
                access: Some(ResourceAccess::Read),
                expected_acl_version: resource.acl_version,
            },
        )
        .await
        .expect("share");
    assert!(
        f.team
            .resource(&reader, &f.org, &resource.resource_id, ResourceAccess::Read)
            .await
            .is_ok()
    );
    assert!(matches!(
        f.team
            .set_membership(
                &admin,
                &f.org,
                "promote-self",
                &ChangeMembership {
                    user_id: admin.clone(),
                    role: Some(TeamRole::Owner),
                    expected_policy_version: version(&f).await
                }
            )
            .await,
        Err(TeamError::Forbidden)
    ));
    let fourth = new_user(&f.pool).await;
    assert!(matches!(
        f.team
            .set_membership(
                &f.owner,
                &f.org,
                "over-capacity",
                &ChangeMembership {
                    user_id: fourth,
                    role: Some(TeamRole::Member),
                    expected_policy_version: version(&f).await
                }
            )
            .await,
        Err(TeamError::Capacity)
    ));
    let remove = ChangeMembership {
        user_id: reader.clone(),
        role: None,
        expected_policy_version: version(&f).await,
    };
    let result = f
        .team
        .set_membership(&admin, &f.org, "remove-viewer", &remove)
        .await
        .expect("admin can remove ordinary member");
    assert!(
        f.team
            .resource(&reader, &f.org, &resource.resource_id, ResourceAccess::Read)
            .await
            .is_err()
    );
    assert_eq!(
        f.team
            .set_membership(&admin, &f.org, "remove-viewer", &remove)
            .await
            .expect("idempotent historical result")
            .policy_version,
        result.policy_version
    );
    assert!(matches!(
        f.team
            .set_membership(
                &admin,
                &f.org,
                "remove-viewer",
                &ChangeMembership {
                    user_id: reader.clone(),
                    role: Some(TeamRole::Member),
                    expected_policy_version: result.policy_version
                }
            )
            .await,
        Err(TeamError::Conflict)
    ));
    add(&f, &reader, TeamRole::Viewer).await;
    assert!(matches!(
        f.team
            .resource(&reader, &f.org, &resource.resource_id, ResourceAccess::Read)
            .await,
        Err(TeamError::Forbidden)
    ));
    assert!(
        sqlx::query("DELETE FROM team_audit WHERE organization_id=$1")
            .bind(&f.org)
            .execute(&f.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn saved_creation_results_do_not_bypass_later_membership_or_content_revocation() {
    let f = fixture(20).await;
    let successor = new_user(&f.pool).await;
    add(&f, &successor, TeamRole::Owner).await;
    let resource = f
        .team
        .register_resource(&f.owner, &f.org, "original-resource", "thread")
        .await
        .expect("resource");
    f.team
        .set_membership(
            &successor,
            &f.org,
            "remove-creator",
            &ChangeMembership {
                user_id: f.owner.clone(),
                role: None,
                expected_policy_version: version(&f).await,
            },
        )
        .await
        .expect("remove former owner");
    assert!(matches!(
        f.team
            .create_organization(
                &f.owner,
                "create-org",
                &CreateOrganization {
                    name: "Local ACL fixture".into(),
                    member_limit: 20
                }
            )
            .await,
        Err(TeamError::Forbidden)
    ));
    f.team
        .set_membership(
            &successor,
            &f.org,
            "rejoin",
            &ChangeMembership {
                user_id: f.owner.clone(),
                role: Some(TeamRole::Member),
                expected_policy_version: version(&f).await,
            },
        )
        .await
        .expect("rejoin as ordinary member");
    assert!(matches!(
        f.team
            .register_resource(&f.owner, &f.org, "original-resource", "thread")
            .await,
        Err(TeamError::Forbidden)
    ));
    assert!(matches!(
        f.team
            .resource(
                &f.owner,
                &f.org,
                &resource.resource_id,
                ResourceAccess::Read
            )
            .await,
        Err(TeamError::Forbidden)
    ));
}
