//! CN-04 signed space navigation and version-bound attachment acquisition.
mod adf;
pub mod registry;
mod wire;
use crate::document_handles::Seals;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use wire::Client;
pub const LIMIT: usize = 2 * 1024 * 1024;
pub const MAX_FILE: usize = 20 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Confluence request")]
    Invalid,
    #[error("Confluence grant required")]
    Denied,
    #[error("content is missing or inactive")]
    Missing,
    #[error("tenant, space or content revision changed")]
    Changed,
    #[error("Confluence credentials refused")]
    Credentials,
    #[error("source rate limited")]
    RateLimited,
    #[error("Confluence content exceeds limit")]
    Limit,
    #[error("unexpected Confluence response")]
    Protocol,
    #[error("Confluence unavailable")]
    Unavailable,
}
impl From<crate::document_handles::Error> for Error {
    fn from(e: crate::document_handles::Error) -> Self {
        use crate::document_handles::Error as E;
        match e {
            E::Invalid => Self::Invalid,
            E::Denied => Self::Denied,
            E::Limit => Self::Limit,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Space {
    pub id: String,
    pub key: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub tenant: String,
    pub cloud_id: String,
    pub spaces: BTreeMap<String, Space>,
    #[serde(default)]
    pub media_hosts: BTreeSet<String>,
}
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Credential {
    SiteToken { email: String, token: String },
    CloudToken { email: String, token: String },
    CloudOauth { token: String },
}
pub struct Scope {
    pub grant: String,
    pub spaces: BTreeSet<String>,
}
pub struct Source {
    registration: Registration,
    client: Client,
    seals: Seals,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    id: String,
    space: String,
    version: u64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub file_id: String,
    pub id: String,
    pub page: String,
    pub space: String,
    pub title: String,
    pub version: u64,
    pub media_type: String,
    pub bytes: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    SearchPages {
        space: String,
        query: String,
        limit: u8,
        cursor: Option<String>,
    },
    ReadPage {
        space: String,
        page: String,
        #[serde(default)]
        attachments_cursor: Option<String>,
    },
    ListChildren {
        space: String,
        page: String,
        limit: u8,
        cursor: Option<String>,
    },
    ReadAttachment {
        space: String,
        attachment: String,
    },
}
pub enum ResultData {
    Document(Value),
    Attachment {
        meta: Attachment,
        bytes: Vec<u8>,
        sha256: String,
    },
}
fn decimal(v: &str) -> bool {
    v.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == v)
}
fn attachment_id(v: &str) -> bool {
    decimal(v.strip_prefix("att").unwrap_or(v))
}
fn version(v: &Value) -> Result<u64, Error> {
    v["version"]["number"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(Error::Protocol)
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str, Error> {
    v[key].as_str().ok_or(Error::Protocol)
}
fn context<T: Serialize>(value: &T) -> Result<String, Error> {
    orange_protocol::request_digest("confluence.navigation.v1", value).map_err(|_| Error::Invalid)
}
impl Source {
    fn space(&self, s: &Scope, alias: &str) -> Result<&Space, Error> {
        if !orange_protocol::valid_id(&s.grant) || !s.spaces.contains(alias) {
            return Err(Error::Denied);
        }
        self.registration.spaces.get(alias).ok_or(Error::Denied)
    }
    pub fn authorization_refused(&self) -> bool {
        self.client.refused()
    }
    pub fn authorize(&self, s: &Scope, a: &Action) -> Result<(), Error> {
        match a {
            Action::SearchPages {
                space,
                query,
                limit,
                cursor,
            } => {
                self.space(s, space)?;
                if query.len() > 1024
                    || query.chars().any(char::is_control)
                    || !(1..=100).contains(limit)
                {
                    return Err(Error::Invalid);
                }
                self.cursor(
                    s,
                    &context(&("search", space, query, limit))?,
                    cursor.as_deref(),
                )?;
            }
            Action::ReadPage {
                space,
                page,
                attachments_cursor,
            } => {
                self.page_handle(s, space, page)?;
                self.cursor(
                    s,
                    &context(&("attachments", space, page))?,
                    attachments_cursor.as_deref(),
                )?;
            }
            Action::ListChildren {
                space,
                page,
                limit,
                cursor,
            } => {
                self.page_handle(s, space, page)?;
                if !(1..=100).contains(limit) {
                    return Err(Error::Invalid);
                }
                self.cursor(
                    s,
                    &context(&("children", space, page, limit))?,
                    cursor.as_deref(),
                )?;
            }
            Action::ReadAttachment { space, attachment } => {
                let m = self.attachment_handle(s, space, attachment)?;
                if m.bytes > MAX_FILE as u64 {
                    return Err(Error::Limit);
                }
            }
        }
        Ok(())
    }
    fn page_handle(&self, s: &Scope, alias: &str, h: &str) -> Result<Page, Error> {
        let space = self.space(s, alias)?;
        let loc: Page = self.seals.open(&s.grant, "page", alias, h)?;
        if loc.space != space.id || !decimal(&loc.id) {
            return Err(Error::Denied);
        }
        Ok(loc)
    }
    fn attachment_handle(&self, s: &Scope, alias: &str, h: &str) -> Result<Attachment, Error> {
        let space = self.space(s, alias)?;
        let m: Attachment = self.seals.open(&s.grant, "attachment", alias, h)?;
        if m.space != space.id || !decimal(&m.page) || !attachment_id(&m.id) {
            return Err(Error::Denied);
        }
        Ok(m)
    }
    fn cursor(&self, s: &Scope, binding: &str, v: Option<&str>) -> Result<Option<String>, Error> {
        v.map(|token| {
            self.seals
                .open(&s.grant, "cursor", binding, token)
                .map_err(Error::from)
        })
        .transpose()
    }
    pub(crate) fn expected_attachment(
        &self,
        s: &Scope,
        a: &Action,
    ) -> Result<Option<Attachment>, Error> {
        match a {
            Action::ReadAttachment { space, attachment } => {
                Ok(Some(self.attachment_handle(s, space, attachment)?))
            }
            _ => Ok(None),
        }
    }
    pub(crate) async fn current_attachment<F, Fut>(
        &self,
        s: &Scope,
        alias: &str,
        meta: &Attachment,
        check: F,
    ) -> Result<(), Error>
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<(), Error>> + Send,
    {
        let space = self.space(s, alias)?;
        if space.id != meta.space {
            return Err(Error::Denied);
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            self.client
                .verify_tenant(&self.registration, &check)
                .await?;
            self.client.verify_space(space, &check).await?;
            self.client.verify_attachment(space, meta, &check).await?;
            self.client
                .verify_tenant(&self.registration, &check)
                .await?;
            self.client.verify_space(space, &check).await
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
    pub async fn execute<F, Fut>(&self, s: &Scope, a: Action, admit: F) -> Result<ResultData, Error>
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<(), Error>> + Send,
    {
        self.authorize(s, &a)?;
        tokio::time::timeout(Duration::from_secs(30), self.read(s, a, &admit))
            .await
            .map_err(|_| Error::Unavailable)?
    }
    async fn read<F, Fut>(&self, s: &Scope, a: Action, admit: &F) -> Result<ResultData, Error>
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<(), Error>> + Send,
    {
        let alias = match &a {
            Action::SearchPages { space, .. }
            | Action::ReadPage { space, .. }
            | Action::ListChildren { space, .. }
            | Action::ReadAttachment { space, .. } => space.clone(),
        };
        let space = self.space(s, &alias)?;
        self.client.verify_tenant(&self.registration, admit).await?;
        self.client.verify_space(space, admit).await?;
        let v = match a {
            Action::SearchPages {
                query,
                limit,
                cursor,
                ..
            } => {
                let binding = context(&("search", &alias, &query, limit))?;
                let cur = self.cursor(s, &binding, cursor.as_deref())?;
                let (rows, next) = self
                    .client
                    .search(space, &query, limit, cur.as_deref(), admit)
                    .await?;
                let mut pages = Vec::new();
                for id in rows {
                    let raw = self.client.page(space, &id, false, admit).await?;
                    pages.push(self.page_summary(s, &alias, space, &raw)?);
                }
                json!({"pages":pages,"next_cursor":self.next(s,&binding,next)?})
            }
            Action::ReadPage {
                page,
                attachments_cursor,
                ..
            } => {
                let loc = self.page_handle(s, &alias, &page)?;
                let raw = self.client.page(space, &loc.id, true, admit).await?;
                if version(&raw)? != loc.version {
                    return Err(Error::Changed);
                }
                let (text, omitted) = adf::text(&raw["body"]["atlas_doc_format"])?;
                let binding = context(&("attachments", &alias, &page))?;
                let cur = self.cursor(s, &binding, attachments_cursor.as_deref())?;
                let (rows, next) = self
                    .client
                    .attachments(&loc.id, cur.as_deref(), admit)
                    .await?;
                let mut attachments = Vec::new();
                for v in &rows {
                    let m = wire::attachment(v, space, &loc.id)?;
                    let handle = self.seals.seal(&s.grant, "attachment", &alias, &m)?;
                    attachments.push(json!({"id":m.id,"title":m.title,"media_type":m.media_type,"size_bytes":m.bytes.to_string(),"version":m.version.to_string(),"handle":handle}));
                }
                self.client.verify_page(&loc, space, admit).await?;
                json!({"page":self.page_summary(s,&alias,space,&raw)?,"text":text,"omitted_nodes":omitted,"attachments":attachments,"next_attachments_cursor":self.next(s,&binding,next)?})
            }
            Action::ListChildren {
                page,
                limit,
                cursor,
                ..
            } => {
                let loc = self.page_handle(s, &alias, &page)?;
                self.client.verify_page(&loc, space, admit).await?;
                let binding = context(&("children", &alias, &page, limit))?;
                let cur = self.cursor(s, &binding, cursor.as_deref())?;
                let (rows, next) = self
                    .client
                    .children(&loc.id, limit, cur.as_deref(), admit)
                    .await?;
                let mut children = Vec::new();
                for row in rows {
                    let kind = field(&row, "type")?;
                    let child = field(&row, "id")?;
                    if !decimal(child) {
                        return Err(Error::Protocol);
                    }
                    if kind == "page" {
                        let raw = self.client.page(space, child, false, admit).await?;
                        if raw["parentId"] != loc.id {
                            return Err(Error::Changed);
                        }
                        children.push(self.page_summary(s, &alias, space, &raw)?);
                    } else {
                        children.push(json!({"id":child,"type":kind,"title":field(&row,"title")?}));
                    }
                }
                self.client.verify_page(&loc, space, admit).await?;
                json!({"children":children,"next_cursor":self.next(s,&binding,next)?})
            }
            Action::ReadAttachment { attachment, .. } => {
                let expected = self.attachment_handle(s, &alias, &attachment)?;
                let data = self
                    .client
                    .read_attachment(&self.registration, space, &expected, admit)
                    .await?;
                self.client.verify_tenant(&self.registration, admit).await?;
                self.client.verify_space(space, admit).await?;
                return Ok(ResultData::Attachment {
                    meta: expected,
                    sha256: hex::encode(Sha256::digest(&data)),
                    bytes: data,
                });
            }
        };
        self.client.verify_space(space, admit).await?;
        self.client.verify_tenant(&self.registration, admit).await?;
        let out = json!({"kind":"external_untrusted_data","data":v});
        if serde_json::to_vec(&out).map_err(|_| Error::Protocol)?.len() > LIMIT {
            return Err(Error::Limit);
        }
        Ok(ResultData::Document(out))
    }
    fn next(
        &self,
        s: &Scope,
        binding: &str,
        next: Option<String>,
    ) -> Result<Option<String>, Error> {
        next.map(|n| {
            self.seals
                .seal(&s.grant, "cursor", binding, n)
                .map_err(Error::from)
        })
        .transpose()
    }
    fn page_summary(
        &self,
        s: &Scope,
        alias: &str,
        space: &Space,
        raw: &Value,
    ) -> Result<Value, Error> {
        let page = Page {
            id: field(raw, "id")?.into(),
            space: space.id.clone(),
            version: version(raw)?,
        };
        Ok(
            json!({"type":"page","id":page.id,"title":field(raw,"title")?,"version":page.version.to_string(),"handle":self.seals.seal(&s.grant,"page",alias,&page)?}),
        )
    }
}

#[cfg(any(test, feature = "confluence-fixture-tests"))]
#[doc(hidden)]
pub mod fixtures;
#[cfg(test)]
mod tests;
