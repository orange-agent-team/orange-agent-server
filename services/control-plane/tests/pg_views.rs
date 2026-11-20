use orange_control_plane::pg_views::{Action, Error, Registration, View};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};

fn action(value: Value) -> Action {
    serde_json::from_value(value).expect("typed action")
}
fn page(columns: Value, after: Option<i64>) -> Action {
    action(
        json!({"action":"query_view","view_id":"sales","columns":columns,"after":after,"limit":1}),
    )
}

#[tokio::test]
async fn approved_source_view_uses_actual_read_role_and_bounded_queries() {
    let admin = PgPool::connect(&std::env::var("ORANGE_VIEW_FIXTURE_ADMIN").expect("owned admin"))
        .await
        .expect("admin pool");
    let reader =
        PgPool::connect(&std::env::var("ORANGE_VIEW_FIXTURE_READER").expect("owned reader"))
            .await
            .expect("read pool");
    let metadata = sqlx::query("SELECT current_database() AS db,current_user AS role")
        .fetch_one(&reader)
        .await
        .expect("source identity");
    let def: String = sqlx::query_scalar("SELECT pg_get_viewdef('source.sales'::regclass,false)")
        .fetch_one(&admin)
        .await
        .expect("source view");
    let view = View {
        schema: "source".into(),
        relation: "sales".into(),
        definition_sha256: hex::encode(Sha256::digest(def.as_bytes())),
        columns: BTreeMap::from([
            ("seq".into(), "int8".into()),
            ("name".into(), "text".into()),
        ]),
        paging_key: "seq".into(),
    };
    let registry = Registration {
        database: metadata.get("db"),
        read_role: metadata.get("role"),
        views: BTreeMap::from([("sales".into(), view)]),
    };
    let grant = BTreeSet::from(["sales".into()]);
    let listed = registry
        .execute(&reader, &grant, Action::ListViews)
        .await
        .expect("authorized list");
    assert_eq!(listed["data"]["view_ids"], json!(["sales"]));
    let description = registry
        .execute(
            &reader,
            &grant,
            Action::DescribeView {
                view_id: "sales".into(),
            },
        )
        .await
        .expect("authorized description");
    assert_eq!(
        description["data"]["columns"],
        json!({"name":"text","seq":"int8"})
    );
    let batch = registry
        .execute(&reader, &grant, page(json!(["name"]), None))
        .await
        .expect("first page");
    assert_eq!(
        batch,
        json!({"kind":"external_untrusted_data","data":{"rows":[{"name":"橙子"}],"next_after":10}})
    );
    let batch = registry
        .execute(&reader, &grant, page(json!(["name"]), Some(10)))
        .await
        .expect("next page");
    assert_eq!(
        batch["data"],
        json!({"rows":[{"name":"pear"}],"next_after":null})
    );
    let filtered = action(
        json!({"action":"query_view","view_id":"sales","columns":["seq"],"equals":{"column":"name","value":"橙子"},"descending":true,"limit":2}),
    );
    assert_eq!(
        registry
            .execute(&reader, &grant, filtered)
            .await
            .expect("bound equality")["data"]["rows"],
        json!([{"seq":10}])
    );
    assert!(serde_json::from_value::<Action>(json!({"action":"query_view","view_id":"sales","sql":"select 1","columns":["name"],"limit":1})).is_err());
    assert!(matches!(
        registry
            .execute(&reader, &BTreeSet::new(), page(json!(["name"]), None))
            .await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        registry.execute(&admin, &grant, Action::ListViews).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        registry
            .execute(&reader, &grant, page(json!(["internal_note"]), None))
            .await,
        Err(Error::Invalid)
    ));
    sqlx::query("UPDATE source.orders SET name=repeat('x',1048576) WHERE seq=10")
        .execute(&admin)
        .await
        .expect("large fixture");
    assert!(matches!(
        registry
            .execute(&reader, &grant, page(json!(["name"]), None))
            .await,
        Err(Error::Limit)
    ));
    let mut ddl = admin.begin().await.expect("fixture DDL transaction");
    sqlx::query(
        "CREATE OR REPLACE VIEW source.sales AS SELECT seq,name FROM source.orders WHERE seq>=20",
    )
    .execute(&mut *ddl)
    .await
    .expect("schema drift");
    let (registry_copy, pool, scope) = (registry.clone(), reader.clone(), grant.clone());
    let pending = tokio::spawn(async move {
        registry_copy
            .execute(&pool, &scope, Action::ListViews)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3),async{
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_stat_activity WHERE usename=$1 AND wait_event_type='Lock')").bind(&registry.read_role).fetch_one(&admin).await.expect("lock observation");
            if waiting{break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("source read blocked on fixture DDL");
    ddl.commit().await.expect("publish changed definition");
    assert!(matches!(
        pending.await.expect("source task"),
        Err(Error::Changed)
    ));
    assert!(matches!(
        registry.execute(&reader, &grant, Action::ListViews).await,
        Err(Error::Changed)
    ));
    sqlx::query("CREATE OR REPLACE VIEW source.sales AS SELECT seq,name FROM source.orders")
        .execute(&admin)
        .await
        .expect("restore approved definition");
    registry
        .execute(&reader, &grant, Action::ListViews)
        .await
        .expect("restored view is readable");
    let role = &registry.read_role;
    assert!(
        role.starts_with("cn05_")
            && role[5..].len() == 32
            && role[5..].bytes().all(|b| b.is_ascii_hexdigit())
    );
    sqlx::QueryBuilder::new("REVOKE SELECT ON source.sales FROM ")
        .push(role)
        .build()
        .execute(&admin)
        .await
        .expect("remove fixture privilege");
    assert!(
        registry
            .execute(&reader, &grant, page(json!(["seq"]), None))
            .await
            .is_err()
    );
    reader.close().await;
    admin.close().await;
}
