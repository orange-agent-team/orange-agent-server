//! CN-05 source-side execution. Registration and the source pool come from the connector host.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use tokio_stream::StreamExt;

const BYTE_LIMIT: usize = 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("source login or read-role privilege refused")]
    SourceAuthority,
    #[error("unregistered connector resource")]
    Denied,
    #[error("registered view or read role changed")]
    Changed,
    #[error("invalid bounded query")]
    Invalid,
    #[error("connector result exceeds byte limit")]
    Limit,
    #[error("registered source unavailable")]
    Unavailable,
}
impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &err {
            match db.code().as_deref() {
                Some("28000" | "28P01" | "42501") => return Self::SourceAuthority,
                Some("42P01" | "42703") => return Self::Changed,
                _ => {}
            }
        }
        Self::Unavailable
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub schema: String,
    pub relation: String,
    pub relation_oid: u32,
    pub definition_sha256: String,
    pub columns: BTreeMap<String, String>,
    pub paging_key: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub database: String,
    pub read_role: String,
    pub views: BTreeMap<String, View>,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    ListViews {},
    DescribeView {
        view_id: String,
    },
    QueryView {
        view_id: String,
        columns: Vec<String>,
        equals: Option<Filter>,
        after: Option<String>,
        #[serde(default)]
        descending: bool,
        limit: u16,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    column: String,
    value: String,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}
impl Registration {
    pub fn validate(&self) -> Result<(), Error> {
        if !identifier(&self.read_role)
            || !identifier(&self.database)
            || !(1..=100).contains(&self.views.len())
        {
            return Err(Error::Invalid);
        }
        for (id, v) in &self.views {
            if !orange_protocol::valid_id(id)
                || !identifier(&v.schema)
                || v.schema.starts_with("pg_")
                || v.schema == "information_schema"
                || v.relation_oid == 0
                || !identifier(&v.relation)
                || !orange_protocol::valid_digest(&v.definition_sha256)
                || !(1..=32).contains(&v.columns.len())
                || v.columns.get(&v.paging_key).map(String::as_str) != Some("int8")
                || v.columns.iter().any(|(key, ty)| {
                    !identifier(key) || !matches!(ty.as_str(), "text" | "int8" | "bool")
                })
            {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }

    /// The host passes only view IDs authorized for this operation. A separate read-only pool is mandatory.
    pub async fn execute(
        &self,
        pool: &PgPool,
        allowed: &BTreeSet<String>,
        action: Action,
    ) -> Result<Value, Error> {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.read(pool, allowed, action),
        )
        .await
        .map_err(|_| Error::Unavailable)?
    }

    async fn read(
        &self,
        pool: &PgPool,
        allowed: &BTreeSet<String>,
        action: Action,
    ) -> Result<Value, Error> {
        self.validate()?;
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ ONLY")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL search_path = pg_catalog")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        let role_ok:bool=sqlx::query_scalar("SELECT current_database()=$1 AND current_user=$2 AND session_user=current_user AND current_setting('transaction_read_only')='on' AND rolcanlogin AND NOT (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls) AND NOT EXISTS(SELECT 1 FROM pg_auth_members WHERE member=r.oid) FROM pg_roles r WHERE rolname=current_user")
            .bind(&self.database).bind(&self.read_role).fetch_one(&mut *tx).await?;
        if !role_ok {
            return Err(Error::SourceAuthority);
        }
        let result = match action {
            Action::ListViews {} => {
                let mut ids = Vec::new();
                for id in allowed {
                    let view = self.views.get(id).ok_or(Error::Denied)?;
                    view.check(&mut tx).await?;
                    ids.push(id);
                }
                json!({"view_ids":ids})
            }
            Action::DescribeView { ref view_id } | Action::QueryView { ref view_id, .. } => {
                if !allowed.contains(view_id) {
                    return Err(Error::Denied);
                }
                let view = self.views.get(view_id).ok_or(Error::Denied)?;
                view.check(&mut tx).await?;
                match action {
                    Action::QueryView {
                        columns,
                        equals,
                        after,
                        descending,
                        limit,
                        ..
                    } => {
                        view.page(&mut tx, &columns, equals, after, descending, limit)
                            .await?
                    }
                    _ => {
                        json!({"view_id":view_id,"columns":view.columns,"paging_key":view.paging_key})
                    }
                }
            }
        };
        tx.commit().await?;
        Ok(json!({"kind":"external_untrusted_data","data":result}))
    }
}
impl View {
    fn relation_sql(&self) -> String {
        format!("\"{}\".\"{}\"", self.schema, self.relation)
    }
    async fn check(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), Error> {
        // Hold a relation read lock while comparing its definition and reading rows.
        let mut lock = QueryBuilder::new("LOCK TABLE ONLY ");
        lock.push(self.relation_sql()).push(" IN ACCESS SHARE MODE");
        lock.build().execute(&mut **tx).await.map_err(Error::from)?;
        let row=sqlx::query("SELECT c.oid::bigint AS oid, pg_get_viewdef(c.oid,false) AS definition, jsonb_object_agg(a.attname,t.typname) AS columns FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped JOIN pg_type t ON t.oid=a.atttypid AND t.typnamespace='pg_catalog'::regnamespace WHERE n.nspname=$1 AND c.relname=$2 AND c.relkind='v' AND c.relowner<>(SELECT oid FROM pg_roles WHERE rolname=current_user) AND has_table_privilege(c.oid,'SELECT') AND NOT has_table_privilege(c.oid,'INSERT,UPDATE,DELETE,TRUNCATE,TRIGGER') GROUP BY c.oid")
            .bind(&self.schema).bind(&self.relation).fetch_optional(&mut **tx).await?.ok_or(Error::Changed)?;
        let definition: String = row.try_get("definition")?;
        let columns: Value = row.try_get("columns")?;
        if row.try_get::<i64, _>("oid")? != i64::from(self.relation_oid)
            || hex::encode(Sha256::digest(definition.as_bytes())) != self.definition_sha256
            || self
                .columns
                .iter()
                .any(|(key, ty)| columns[key].as_str() != Some(ty))
        {
            return Err(Error::Changed);
        }
        Ok(())
    }
    async fn page(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        columns: &[String],
        equals: Option<Filter>,
        after: Option<String>,
        descending: bool,
        limit: u16,
    ) -> Result<Value, Error> {
        if columns.is_empty()
            || columns.len() > self.columns.len()
            || columns.iter().collect::<BTreeSet<_>>().len() != columns.len()
            || columns.iter().any(|c| !self.columns.contains_key(c))
            || !(1..=1000).contains(&limit)
        {
            return Err(Error::Invalid);
        }
        let after = after
            .map(|raw| {
                let n = raw.parse::<i64>().map_err(|_| Error::Invalid)?;
                if raw != n.to_string() {
                    return Err(Error::Invalid);
                }
                Ok(n)
            })
            .transpose()?;
        let key = format!("v.\"{}\"", self.paging_key);
        let mut q = QueryBuilder::new(
            "SELECT page.k, CASE WHEN octet_length(page.data::text)<=1048576 THEN page.data END AS data FROM (SELECT ",
        );
        q.push(&key).push(" AS k, jsonb_build_object(");
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                q.push(",");
            }
            q.push_bind(col)
                .push("::text,")
                .push(format!("v.\"{col}\""));
            if self.columns[col] == "int8" {
                q.push("::text");
            }
        }
        q.push(") AS data FROM ")
            .push(self.relation_sql())
            .push(" AS v WHERE true");
        if let Some(f) = equals {
            if !self.columns.contains_key(&f.column) || f.value.len() > 4096 {
                return Err(Error::Invalid);
            }
            q.push(format!(" AND v.\"{}\"::text=", f.column))
                .push_bind(f.value);
        }
        if let Some(cursor) = after {
            q.push(" AND ")
                .push(&key)
                .push(if descending { " < " } else { " > " })
                .push_bind(cursor);
        }
        let order = if descending {
            " DESC NULLS FIRST"
        } else {
            " ASC NULLS FIRST"
        };
        q.push(" ORDER BY ")
            .push(key)
            .push(order)
            .push(" LIMIT ")
            .push_bind(i64::from(limit) + 1)
            .push(") AS page ORDER BY page.k")
            .push(order);
        let mut stream = q.build().fetch(&mut **tx);
        let mut rows = Vec::new();
        let mut previous = None;
        let mut next = None;
        let mut bytes = 0;
        while let Some(row) = stream.try_next().await? {
            let k = row.try_get::<Option<i64>, _>("k")?.ok_or(Error::Changed)?;
            if previous == Some(k) {
                return Err(Error::Changed);
            }
            if rows.len() == usize::from(limit) {
                next = previous.map(|key: i64| key.to_string());
                break;
            }
            let data = row
                .try_get::<Option<Value>, _>("data")?
                .ok_or(Error::Limit)?;
            bytes += serde_json::to_vec(&data).map_err(|_| Error::Invalid)?.len() + 1;
            if bytes > BYTE_LIMIT {
                return Err(Error::Limit);
            }
            previous = Some(k);
            rows.push(data);
        }
        let result = json!({"rows":rows,"next_after":next});
        if serde_json::to_vec(&result)
            .map_err(|_| Error::Invalid)?
            .len()
            + 64
            > BYTE_LIMIT
        {
            return Err(Error::Limit);
        }
        Ok(result)
    }
}
