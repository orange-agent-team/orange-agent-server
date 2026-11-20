//! CN-03. Only registered pages and registered database/data-source pairs can anchor a traversal.
mod handles;
mod projection;
pub mod registry;
use handles::{Location, Seals};
use reqwest::{Client, StatusCode, Url, header::HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const VERSION: &str = "2026-03-11";
const MAX_BYTES: usize = 2 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Notion request")]
    Invalid,
    #[error("ungranted or expired Notion resource handle")]
    Denied,
    #[error("Notion object inaccessible or in trash")]
    Missing,
    #[error("Notion resource identity or parent changed")]
    Changed,
    #[error("Notion credentials rejected")]
    Credentials,
    #[error("Notion source rate limit")]
    RateLimited,
    #[error("Notion result exceeds limit")]
    Limit,
    #[error("unexpected Notion payload")]
    Protocol,
    #[error("Notion unavailable")]
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Root {
    Page {
        page_id: String,
    },
    Database {
        database_id: String,
        data_sources: Vec<String>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub roots: BTreeMap<String, Root>,
}
pub struct Scope {
    pub grant: String,
    pub allowed: BTreeSet<String>,
}
pub struct Source {
    registration: Registration,
    client: Client,
    origin: Url,
    token: HeaderValue,
    seals: Seals,
    refused: AtomicBool,
    retry_at: AtomicU64,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    SearchPages {
        query: String,
        limit: u8,
        cursor: Option<String>,
    },
    ReadPage {
        root: String,
        page: Option<String>,
    },
    ListBlocks {
        root: String,
        target: Option<String>,
        limit: u8,
        cursor: Option<String>,
    },
    QueryDatabase {
        root: String,
        data_source: String,
        limit: u8,
        cursor: Option<String>,
        filter: Option<Filter>,
        sort: Option<Sort>,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    property: String,
    kind: String,
    equals: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    timestamp: String,
    direction: String,
}
fn id(s: &str) -> Result<String, Error> {
    uuid::Uuid::parse_str(s)
        .ok()
        .filter(|v| !v.is_nil())
        .map(|v| v.to_string())
        .ok_or(Error::Invalid)
}
fn stamp<T: Serialize>(name: &str, v: &T) -> Result<String, Error> {
    orange_protocol::request_digest(name, v).map_err(|_| Error::Invalid)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
impl Registration {
    #[cfg(feature = "notion-fixture-tests")]
    pub fn open_loopback_fixture(
        &self,
        address: std::net::SocketAddr,
        token: &str,
    ) -> Result<Source, Error> {
        if !address.ip().is_loopback() {
            return Err(Error::Denied);
        }
        self.build(
            token,
            Url::parse(&format!("http://{address}/v1")).map_err(|_| Error::Invalid)?,
        )
    }
    pub fn open(&self, token: &str) -> Result<Source, Error> {
        self.build(
            token,
            Url::parse("https://api.notion.com/v1").map_err(|_| Error::Invalid)?,
        )
    }
    fn build(&self, token: &str, origin: Url) -> Result<Source, Error> {
        if !(1..=100).contains(&self.roots.len())
            || !(16..=4096).contains(&token.len())
            || !token.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(Error::Invalid);
        }
        let mut seen = BTreeSet::new();
        let mut normalized = self.clone();
        for (key, root) in &mut normalized.roots {
            if !orange_protocol::valid_id(key) {
                return Err(Error::Invalid);
            }
            match root {
                Root::Page { page_id } => {
                    *page_id = id(page_id)?;
                    if !seen.insert(page_id.clone()) {
                        return Err(Error::Invalid);
                    }
                }
                Root::Database {
                    database_id,
                    data_sources,
                } => {
                    *database_id = id(database_id)?;
                    if !seen.insert(database_id.clone()) || !(1..=32).contains(&data_sources.len())
                    {
                        return Err(Error::Invalid);
                    }
                    for ds in data_sources.iter_mut() {
                        *ds = id(ds)?;
                        if !seen.insert(ds.clone()) {
                            return Err(Error::Invalid);
                        }
                    }
                    data_sources.sort();
                }
            }
        }
        let seals = Seals::new(token, &normalized)?;
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Invalid)?;
        auth.set_sensitive(true);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Error::Unavailable)?;
        Ok(Source {
            registration: normalized,
            client,
            origin,
            token: auth,
            seals,
            refused: AtomicBool::new(false),
            retry_at: AtomicU64::new(0),
        })
    }
}
impl Source {
    pub fn authorization_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }
    fn root<'a>(&'a self, scope: &Scope, key: &str) -> Result<&'a Root, Error> {
        if !scope.allowed.contains(key) {
            return Err(Error::Denied);
        }
        self.registration.roots.get(key).ok_or(Error::Denied)
    }
    fn row_parent(&self, row: &Value, root: &Root, ds: &str) -> Result<(), Error> {
        projection::parent(row, "data_source_id", ds)?;
        let Root::Database { database_id, .. } = root else {
            return Err(Error::Denied);
        };
        if let Some(value) = row["parent"].get("database_id")
            && value.as_str().and_then(|v| id(v).ok()).as_ref() != Some(database_id)
        {
            return Err(Error::Changed);
        }
        Ok(())
    }
    fn location(&self, scope: &Scope, key: &str, handle: Option<&str>) -> Result<Location, Error> {
        let root = self.root(scope, key)?;
        if let Some(handle) = handle {
            return self.seals.location(scope, key, handle);
        }
        match root {
            Root::Page { page_id } => Ok(Location {
                page: page_id.clone(),
                data_source: None,
                blocks: vec![],
            }),
            _ => Err(Error::Denied),
        }
    }
    pub fn authorize(&self, scope: &Scope, a: &Action) -> Result<(), Error> {
        if !orange_protocol::valid_id(&scope.grant)
            || scope.allowed.is_empty()
            || scope
                .allowed
                .iter()
                .any(|k| !self.registration.roots.contains_key(k))
        {
            return Err(Error::Denied);
        }
        let check_limit = |n: u8| {
            if (1..=100).contains(&n) {
                Ok(())
            } else {
                Err(Error::Invalid)
            }
        };
        match a {
            Action::SearchPages {
                query,
                limit,
                cursor,
            } => {
                check_limit(*limit)?;
                if query.len() > 1024 {
                    return Err(Error::Invalid);
                }
                let binding = stamp(
                    "notion.search.v1",
                    &(&scope.allowed, query.to_lowercase(), limit),
                )?;
                self.seals.cursor(scope, &binding, cursor.as_deref())?;
            }
            Action::ReadPage { root, page } => {
                if !self
                    .location(scope, root, page.as_deref())?
                    .blocks
                    .is_empty()
                {
                    return Err(Error::Denied);
                }
            }
            Action::ListBlocks {
                root,
                target,
                limit,
                cursor,
            } => {
                check_limit(*limit)?;
                let loc = self.location(scope, root, target.as_deref())?;
                if loc.blocks.len() >= 16 {
                    return Err(Error::Limit);
                }
                let binding = stamp("notion.blocks.v1", &(root, loc, limit))?;
                self.seals.cursor(scope, &binding, cursor.as_deref())?;
            }
            Action::QueryDatabase {
                root,
                data_source,
                limit,
                cursor,
                filter,
                sort,
            } => {
                check_limit(*limit)?;
                let source = id(data_source)?;
                let Root::Database { data_sources, .. } = self.root(scope, root)? else {
                    return Err(Error::Denied);
                };
                if !data_sources.contains(&source) {
                    return Err(Error::Denied);
                }
                if let Some(f) = filter {
                    projection::filter(f)?;
                }
                if let Some(s) = sort
                    && (!matches!(s.timestamp.as_str(), "created_time" | "last_edited_time")
                        || !matches!(s.direction.as_str(), "ascending" | "descending"))
                {
                    return Err(Error::Invalid);
                }
                let binding = stamp("notion.query.v1", &(root, source, filter, sort, limit))?;
                self.seals.cursor(scope, &binding, cursor.as_deref())?;
            }
        }
        Ok(())
    }
    pub async fn execute(&self, scope: &Scope, a: Action) -> Result<Value, Error> {
        self.execute_guarded(scope, a, || async { Ok(()) }).await
    }
    pub(crate) async fn execute_guarded<F, Fut>(
        &self,
        scope: &Scope,
        a: Action,
        check: F,
    ) -> Result<Value, Error>
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<(), Error>> + Send,
    {
        self.authorize(scope, &a)?;
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            Reader {
                source: self,
                check,
            }
            .execute_inner(scope, a),
        )
        .await
        .map_err(|_| Error::Unavailable)??;
        let envelope = json!({"kind":"external_untrusted_data","data":out});
        if serde_json::to_vec(&envelope)
            .map_err(|_| Error::Protocol)?
            .len()
            > MAX_BYTES
        {
            return Err(Error::Limit);
        }
        Ok(envelope)
    }
}
struct Reader<'a, F> {
    source: &'a Source,
    check: F,
}
impl<F> std::ops::Deref for Reader<'_, F> {
    type Target = Source;
    fn deref(&self) -> &Source {
        self.source
    }
}
impl<F, Fut> Reader<'_, F>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<(), Error>> + Send,
{
    async fn request(
        &self,
        segments: &[&str],
        params: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, Error> {
        (self.check)().await?;
        if self.authorization_refused() {
            return Err(Error::Credentials);
        }
        if now() < self.retry_at.load(Ordering::Acquire) {
            return Err(Error::RateLimited);
        }
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| Error::Invalid)?
            .pop_if_empty()
            .extend(segments.iter().copied());
        if !params.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let req = if let Some(body) = body {
            self.client.post(url).json(&body)
        } else {
            self.client.get(url)
        };
        let mut reply = req
            .header("Notion-Version", VERSION)
            .header("Authorization", self.token.clone())
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        match reply.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.refused.store(true, Ordering::Release);
                return Err(Error::Credentials);
            }
            StatusCode::NOT_FOUND => return Err(Error::Missing),
            StatusCode::TOO_MANY_REQUESTS => {
                let delay = reply
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60)
                    .max(1);
                self.retry_at
                    .fetch_max(now().saturating_add(delay), Ordering::AcqRel);
                return Err(Error::RateLimited);
            }
            status if status.is_redirection() => return Err(Error::Changed),
            _ => return Err(Error::Unavailable),
        }
        if reply.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(Error::Limit);
        }
        let mut buf = Vec::new();
        while let Some(chunk) = reply.chunk().await.map_err(|_| Error::Unavailable)? {
            if chunk.len() > MAX_BYTES - buf.len() {
                return Err(Error::Limit);
            }
            buf.extend_from_slice(&chunk);
        }
        if self.authorization_refused() {
            return Err(Error::Credentials);
        }
        serde_json::from_slice(&buf).map_err(|_| Error::Protocol)
    }
    async fn object(&self, kind: &str, identifier: &str) -> Result<Value, Error> {
        let path = match kind {
            "page" => "pages",
            "database" => "databases",
            "block" => "blocks",
            _ => return Err(Error::Invalid),
        };
        let v = self.request(&[path, identifier], &[], None).await?;
        projection::object(&v, kind, identifier)?;
        Ok(v)
    }
    async fn database(&self, root: &Root, ds: &str) -> Result<(), Error> {
        let Root::Database {
            database_id,
            data_sources,
        } = root
        else {
            return Err(Error::Denied);
        };
        if !data_sources.iter().any(|v| v == ds) {
            return Err(Error::Denied);
        }
        let db = self.object("database", database_id).await?;
        if !db["data_sources"]
            .as_array()
            .ok_or(Error::Protocol)?
            .iter()
            .any(|v| v["id"].as_str().and_then(|s| id(s).ok()).as_deref() == Some(ds))
        {
            return Err(Error::Changed);
        }
        Ok(())
    }
    async fn page_at(&self, scope: &Scope, key: &str, loc: &Location) -> Result<Value, Error> {
        let root = self.root(scope, key)?;
        match (root, &loc.data_source) {
            (Root::Page { page_id }, None) if *page_id == loc.page => {}
            (Root::Database { .. }, Some(ds)) => self.database(root, ds).await?,
            _ => return Err(Error::Denied),
        }
        let v = self.object("page", &loc.page).await?;
        if let Some(ds) = &loc.data_source {
            self.row_parent(&v, root, ds)?;
        }
        Ok(v)
    }
    async fn verify_path(&self, scope: &Scope, key: &str, loc: &Location) -> Result<String, Error> {
        self.page_at(scope, key, loc).await?;
        let mut prev = &loc.page;
        for (index, block) in loc.blocks.iter().enumerate() {
            let value = self.object("block", block).await?;
            projection::parent(
                &value,
                if index == 0 { "page_id" } else { "block_id" },
                prev,
            )?;
            if !projection::traversable(&value)? {
                return Err(Error::Changed);
            }
            prev = block;
        }
        Ok(prev.clone())
    }
    /// Validate the user-selected roots and the exact navigation/query shape before charging a Run slot.
    async fn execute_inner(&self, scope: &Scope, a: Action) -> Result<Value, Error> {
        match a {
            Action::SearchPages {
                query,
                limit,
                cursor,
            } => {
                if query.len() > 1024 || !(1..=100).contains(&limit) {
                    return Err(Error::Invalid);
                }
                let query = query.to_lowercase();
                let binding = stamp("notion.search.v1", &(&scope.allowed, &query, limit))?;
                let offset = self
                    .seals
                    .cursor(scope, &binding, cursor.as_deref())?
                    .map(|s| s.parse::<usize>().map_err(|_| Error::Denied))
                    .transpose()?
                    .unwrap_or(0);
                let pages: Vec<_> = scope
                    .allowed
                    .iter()
                    .filter_map(|k| match self.root(scope, k).ok()? {
                        Root::Page { page_id } => Some((k, page_id)),
                        _ => None,
                    })
                    .collect();
                let mut data = Vec::new();
                let mut observed = Vec::new();
                let mut size = 0;
                for (key, page_id) in pages.iter().skip(offset).take(limit as usize) {
                    let page = self.object("page", page_id).await?;
                    let summary = projection::summary(&page)?;
                    if summary["title"]
                        .as_str()
                        .ok_or(Error::Protocol)?
                        .to_lowercase()
                        .contains(&query)
                    {
                        size += serde_json::to_vec(&summary)
                            .map_err(|_| Error::Protocol)?
                            .len();
                        if size > MAX_BYTES {
                            return Err(Error::Limit);
                        }
                        observed.push((*page_id, summary["last_edited_time"].clone()));
                        data.push(json!({"root":key,"page":summary}));
                    }
                }
                for (page, edited) in observed {
                    if self.object("page", page).await?["last_edited_time"] != edited {
                        return Err(Error::Changed);
                    }
                }
                let end = offset.saturating_add(limit as usize);
                let next = if end < pages.len() {
                    Some(self.seals.next(scope, &binding, &end.to_string())?)
                } else {
                    None
                };
                Ok(json!({"pages":data,"next_cursor":next}))
            }
            Action::ReadPage { root, page } => {
                let loc = self.location(scope, &root, page.as_deref())?;
                if !loc.blocks.is_empty() {
                    return Err(Error::Denied);
                }
                let item = self.page_at(scope, &root, &loc).await?;
                let doc = projection::page(&item)?;
                if self.page_at(scope, &root, &loc).await?["last_edited_time"]
                    != doc["last_edited_time"]
                {
                    return Err(Error::Changed);
                }
                Ok(doc)
            }
            Action::ListBlocks {
                root,
                target,
                limit,
                cursor,
            } => {
                if !(1..=100).contains(&limit) {
                    return Err(Error::Invalid);
                }
                let loc = self.location(scope, &root, target.as_deref())?;
                if loc.blocks.len() >= 16 {
                    return Err(Error::Limit);
                }
                let parent = self.verify_path(scope, &root, &loc).await?;
                let binding = stamp("notion.blocks.v1", &(&root, &loc, limit))?;
                let mut params = vec![("page_size", limit.to_string())];
                if let Some(cursor) = self.seals.cursor(scope, &binding, cursor.as_deref())? {
                    params.push(("start_cursor", cursor));
                }
                let result = self
                    .request(&["blocks", &parent, "children"], &params, None)
                    .await?;
                let (blocks, next) = projection::list(&result, limit)?;
                let mut data = Vec::new();
                let mut seen = BTreeSet::new();
                for v in blocks {
                    let block = projection::identifier(v)?;
                    if !seen.insert(block.clone()) {
                        return Err(Error::Protocol);
                    }
                    projection::object(v, "block", &block)?;
                    projection::parent(
                        v,
                        if loc.blocks.is_empty() {
                            "page_id"
                        } else {
                            "block_id"
                        },
                        &parent,
                    )?;
                    let mut out = projection::block(v)?;
                    if projection::traversable(v)? {
                        let mut child = loc.clone();
                        child.blocks.push(block);
                        out["children"] = json!(self.seals.child(scope, &root, &child)?);
                    }
                    data.push(out);
                }
                self.verify_path(scope, &root, &loc).await?;
                let next = next
                    .map(|c| self.seals.next(scope, &binding, c))
                    .transpose()?;
                Ok(json!({"blocks":data,"next_cursor":next}))
            }
            Action::QueryDatabase {
                root,
                data_source,
                limit,
                cursor,
                filter,
                sort,
            } => {
                if !(1..=100).contains(&limit) {
                    return Err(Error::Invalid);
                }
                let ds = id(&data_source)?;
                let resource = self.root(scope, &root)?;
                let binding = stamp("notion.query.v1", &(&root, &ds, &filter, &sort, limit))?;
                let mut body = json!({"page_size":limit});
                if let Some(f) = filter {
                    body["filter"] = projection::filter(&f)?;
                }
                if let Some(order) = sort {
                    if !matches!(
                        order.timestamp.as_str(),
                        "created_time" | "last_edited_time"
                    ) || !matches!(order.direction.as_str(), "ascending" | "descending")
                    {
                        return Err(Error::Invalid);
                    }
                    body["sorts"] =
                        json!([{"timestamp":order.timestamp,"direction":order.direction}]);
                }
                if let Some(cursor) = self.seals.cursor(scope, &binding, cursor.as_deref())? {
                    body["start_cursor"] = json!(cursor);
                }
                self.database(resource, &ds).await?;
                let result = self
                    .request(&["data_sources", &ds, "query"], &[], Some(body))
                    .await?;
                let (rows, next) = projection::list(&result, limit)?;
                let mut data = Vec::new();
                let mut seen = BTreeSet::new();
                for row in rows {
                    let page = projection::identifier(row)?;
                    if !seen.insert(page.clone()) {
                        return Err(Error::Protocol);
                    }
                    projection::object(row, "page", &page)?;
                    self.row_parent(row, resource, &ds)?;
                    let loc = Location {
                        page,
                        data_source: Some(ds.clone()),
                        blocks: Vec::new(),
                    };
                    data.push(json!({"handle":self.seals.child(scope,&root,&loc)?,"page":projection::page(row)?}));
                }
                self.database(resource, &ds).await?;
                let next = next
                    .map(|c| self.seals.next(scope, &binding, c))
                    .transpose()?;
                Ok(json!({"rows":data,"next_cursor":next}))
            }
        }
    }
}

#[cfg(test)]
mod tests;
