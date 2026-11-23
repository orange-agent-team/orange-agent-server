//! Each test is invoked on its own database by check-storage-contract.py.
use orange_control_plane::{events::EventStore, store::Store};
use serde::Deserialize;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relation {
    key: Vec<String>,
    #[serde(default)]
    unique: Vec<Vec<String>>,
    references: BTreeMap<String, String>,
}

#[tokio::test]
async fn migrated_database_exposes_the_declared_transaction_keys_and_indexes() {
    let store = Store::connect(&std::env::var("ORANGE_TEST_DATABASE_URL").expect("local DB"))
        .await
        .expect("connect");
    store.migrate().await.expect("all migrations");
    let dictionary: BTreeMap<String, Relation> =
        serde_json::from_str(include_str!("../../../docs/database/transaction-keys.json"))
            .expect("catalogue");
    for (table, relation) in dictionary {
        let keys: Vec<(String, Vec<String>)> = sqlx::query_as(
            "SELECT c.contype::text, array_agg(a.attname::text ORDER BY n.pos)
             FROM pg_constraint c
             CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY n(num,pos)
             JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=n.num
             WHERE c.conrelid=to_regclass($1) AND c.contype IN ('p','u') AND c.convalidated
             GROUP BY c.oid,c.contype",
        )
        .bind(&table)
        .fetch_all(store.pool())
        .await
        .expect("keys");
        assert!(
            keys.contains(&("p".to_owned(), relation.key)),
            "missing primary key: {table}"
        );
        for key in relation.unique {
            assert!(
                keys.contains(&("u".to_owned(), key.clone())),
                "missing unique key: {table} {key:?}"
            );
        }
        for (column, reference) in relation.references {
            let (to, target) = reference.split_once('.').expect("qualified reference");
            let fk: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM pg_constraint c
                    JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=c.conkey[1]
                    JOIN pg_attribute b ON b.attrelid=c.confrelid AND b.attnum=c.confkey[1]
                    WHERE c.contype='f' AND c.convalidated AND cardinality(c.conkey)=1
                      AND c.conrelid=to_regclass($1) AND a.attname=$2
                      AND c.confrelid=to_regclass($3) AND b.attname=$4)",
            )
            .bind(&table)
            .bind(&column)
            .bind(to)
            .bind(target)
            .fetch_one(store.pool())
            .await
            .expect("validated FK");
            assert!(fk, "missing FK: {table}.{column} -> {reference}");
        }
    }
    for (index, table, columns, partial) in [
        (
            "ledger_owner_posted_idx",
            "ledger_transactions",
            vec!["owner_id", "created_at", "transaction_id"],
            false,
        ),
        (
            "artifact_staging_reclaim_idx",
            "artifacts",
            vec!["root_id", "created_at", "artifact_id"],
            true,
        ),
        (
            "inbox_ready",
            "inbox",
            vec!["consumer_id", "state", "available_at"],
            false,
        ),
    ] {
        let (fields, has_predicate): (Vec<String>, bool) = sqlx::query_as(
            "SELECT array_agg(a.attname::text ORDER BY n.pos), i.indpred IS NOT NULL
             FROM pg_index i
             CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY n(num,pos)
             JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=n.num
             WHERE i.indexrelid=to_regclass($1) AND i.indrelid=to_regclass($2)
               AND i.indisvalid AND i.indisready
             GROUP BY i.indexrelid,(i.indpred IS NOT NULL)",
        )
        .bind(index)
        .bind(table)
        .fetch_one(store.pool())
        .await
        .expect("usable index");
        assert_eq!(fields, columns, "{index}");
        assert_eq!(has_predicate, partial, "{index}");
    }
}

#[tokio::test]
async fn schema68_history_upgrades_in_place_and_duplicate_business_keys_still_rollback() {
    let store = Store::connect(
        &std::env::var("ORANGE_TEST_RETENTION_UPGRADE_DATABASE_URL").expect("isolated upgrade DB"),
    )
    .await
    .expect("connect");
    let empty: bool = sqlx::query_scalar("SELECT to_regclass('public.jobs') IS NULL")
        .fetch_one(store.pool())
        .await
        .expect("empty database");
    assert!(empty);
    let stage = tempfile::tempdir().expect("schema 68 migration inputs");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut n = 0;
    for entry in std::fs::read_dir(source).expect("directory") {
        let entry = entry.expect("file");
        let filename = entry.file_name();
        let name = filename.to_str().expect("name");
        if name.ends_with(".sql")
            && name
                .split('_')
                .next()
                .expect("prefix")
                .parse::<u32>()
                .expect("version")
                <= 68
        {
            std::fs::copy(entry.path(), stage.path().join(filename)).expect("copy original SQL");
            n += 1;
        }
    }
    assert_eq!(n, 68);
    sqlx::migrate::Migrator::new(stage.path())
        .await
        .expect("historic inputs")
        .run(store.pool())
        .await
        .expect("legacy database");
    let owner = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO owners(owner_id) VALUES($1)")
        .bind(&owner)
        .execute(store.pool())
        .await
        .expect("owner");
    sqlx::query("INSERT INTO wallets(owner_id,gift) VALUES($1,100)")
        .bind(&owner)
        .execute(store.pool())
        .await
        .expect("wallet");
    let request = orange_protocol::CreateBudget {
        scope_id: "stable-key".into(),
        limit: "45".to_string().try_into().expect("amount"),
    };
    let budget = store
        .create_budget(&owner, "historic-key", &request)
        .await
        .expect("old budget and outbox");
    let history = EventStore::new(store.pool().clone());
    let stream = history
        .discover(&owner, "budget", &budget.root_budget_id)
        .await
        .expect("prior stream");
    let original = history
        .read(&owner, &stream.stream_id, "0", 8)
        .await
        .expect("prior events")
        .events;
    assert_eq!(original.len(), 1);
    store.migrate().await.expect("upgrade");
    assert_eq!(
        store
            .create_budget(&owner, "historic-key", &request)
            .await
            .expect("same key")
            .root_budget_id,
        budget.root_budget_id
    );
    let replay = history
        .read(&owner, &stream.stream_id, "0", 8)
        .await
        .expect("after migration");
    assert_eq!(replay.events[0].event_id, original[0].event_id);
    assert_eq!(replay.events[0].payload_digest, original[0].payload_digest);
    let pending = history
        .prune_delivered_batch(&stream.stream_id, 1, 1)
        .await
        .expect("pending outbox");
    assert_eq!((pending.removed, pending.blocked_at), (0, Some(1)));
    let mut tx = store.pool().begin().await.expect("conflict transaction");
    sqlx::query("UPDATE wallets SET gift=gift-1 WHERE owner_id=$1")
        .bind(&owner)
        .execute(&mut *tx)
        .await
        .expect("tentative effect");
    let collision=sqlx::query("INSERT INTO budget_roots(root_budget_id,owner_id,scope_id,limit_amount) VALUES($1,$2,'stable-key',9)")
        .bind(Uuid::new_v4().to_string()).bind(&owner).execute(&mut *tx).await.expect_err("schema unique key");
    assert_eq!(
        collision
            .as_database_error()
            .and_then(|err| err.code())
            .as_deref(),
        Some("23505")
    );
    tx.rollback().await.expect("rollback");
    let gift: i64 = sqlx::query_scalar("SELECT gift FROM wallets WHERE owner_id=$1")
        .bind(&owner)
        .fetch_one(store.pool())
        .await
        .expect("original funds");
    assert_eq!(gift, 100);
}
