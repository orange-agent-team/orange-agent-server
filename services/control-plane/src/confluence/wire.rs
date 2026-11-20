use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{StatusCode, Url, header::HeaderValue};
use std::{
    future::Future,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) struct Client {
    http: reqwest::Client,
    site: Url,
    api: Url,
    auth: HeaderValue,
    media: Vec<String>,
    rejected: AtomicBool,
    retry_at: AtomicU64,
}
fn origin(u: &Url) -> String {
    u.origin().ascii_serialization()
}
fn domain_label(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
fn media_host(s: &str) -> bool {
    s.len() <= 253
        && s.split('.').all(domain_label)
        && (s == "api.media.atlassian.com"
            || s.ends_with(".media.atlassian.com")
            || s.ends_with(".atlassianusercontent.com")
            || s.ends_with(".atl-paas.net"))
}
fn url_path(base: &Url, parts: &[&str]) -> Result<Url, Error> {
    let mut u = base.clone();
    u.path_segments_mut()
        .map_err(|_| Error::Invalid)?
        .pop_if_empty()
        .extend(parts.iter().copied());
    Ok(u)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
impl Registration {
    pub fn open(&self, credential: &Credential) -> Result<Source, Error> {
        if !self
            .tenant
            .strip_suffix(".atlassian.net")
            .is_some_and(domain_label)
            || self.media_hosts.len() > 12
            || !self.media_hosts.iter().all(|s| media_host(s))
        {
            return Err(Error::Invalid);
        }
        let site = Url::parse(&format!("https://{}", self.tenant)).map_err(|_| Error::Invalid)?;
        let cloud = uuid::Uuid::parse_str(&self.cloud_id).map_err(|_| Error::Invalid)?;
        if cloud.is_nil() || cloud.to_string() != self.cloud_id {
            return Err(Error::Invalid);
        }
        let api = match credential {
            Credential::SiteToken { .. } => url_path(&site, &["wiki"])?,
            _ => Url::parse(&format!(
                "https://api.atlassian.com/ex/confluence/{cloud}/wiki"
            ))
            .map_err(|_| Error::Invalid)?,
        };
        self.build(
            site,
            api,
            self.media_hosts
                .iter()
                .map(|h| format!("https://{h}"))
                .collect(),
            credential,
        )
    }
    #[cfg(any(test, feature = "confluence-fixture-tests"))]
    pub fn open_loopback_fixture(
        &self,
        site: std::net::SocketAddr,
        media: std::net::SocketAddr,
        credential: &Credential,
    ) -> Result<Source, Error> {
        if !site.ip().is_loopback() || !media.ip().is_loopback() || site == media {
            return Err(Error::Invalid);
        }
        let site = Url::parse(&format!("http://{site}")).map_err(|_| Error::Invalid)?;
        let api = url_path(&site, &["wiki"])?;
        self.build(site, api, vec![format!("http://{media}")], credential)
    }
    fn build(
        &self,
        site: Url,
        api: Url,
        media: Vec<String>,
        credential: &Credential,
    ) -> Result<Source, Error> {
        let mut ids = BTreeSet::new();
        let mut keys = BTreeSet::new();
        if !(1..=100).contains(&self.spaces.len())
            || self.spaces.iter().any(|(k, s)| {
                !orange_protocol::valid_id(k)
                    || !decimal(&s.id)
                    || s.key.is_empty()
                    || s.key.len() > 255
                    || s.key.chars().any(char::is_control)
                    || !ids.insert(&s.id)
                    || !keys.insert(&s.key)
            })
        {
            return Err(Error::Invalid);
        }
        let (token, mail) = match credential {
            Credential::SiteToken { email, token } | Credential::CloudToken { email, token } => {
                (token, Some(email))
            }
            Credential::CloudOauth { token } => (token, None),
        };
        if !(8..=8192).contains(&token.len()) || !token.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(Error::Invalid);
        }
        let header = if let Some(mail) = mail {
            if !mail.contains('@')
                || mail.contains(':')
                || mail.len() > 320
                || !mail.bytes().all(|c| c.is_ascii_graphic())
            {
                return Err(Error::Invalid);
            }
            format!("Basic {}", STANDARD.encode(format!("{mail}:{token}")))
        } else {
            format!("Bearer {token}")
        };
        let mut auth = HeaderValue::from_str(&header).map_err(|_| Error::Invalid)?;
        auth.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::Unavailable)?;
        Ok(Source {
            registration: self.clone(),
            seals: Seals::new(b"orange.confluence.navigation.v1\0", token, self)?,
            client: Client {
                http,
                site,
                api,
                auth,
                media,
                rejected: AtomicBool::new(false),
                retry_at: AtomicU64::new(0),
            },
        })
    }
}
impl Client {
    pub fn refused(&self) -> bool {
        self.rejected.load(Ordering::Acquire)
    }
    async fn send<F, Fut>(
        &self,
        url: Url,
        auth: bool,
        json: bool,
        check: &F,
    ) -> Result<reqwest::Response, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        check().await?;
        if self.refused() {
            return Err(Error::Credentials);
        }
        if now() < self.retry_at.load(Ordering::Acquire) {
            return Err(Error::RateLimited);
        }
        if auth
            && (url.origin() != self.api.origin()
                || !url
                    .path()
                    .strip_prefix(self.api.path())
                    .is_some_and(|tail| tail.starts_with('/')))
        {
            return Err(Error::Denied);
        }
        let mut req = self.http.get(url).header(
            "Accept",
            if json {
                "application/json"
            } else {
                "application/octet-stream"
            },
        );
        if auth {
            req = req.header("Authorization", self.auth.clone());
        }
        let r = req.send().await.map_err(|_| Error::Unavailable)?;
        match r.status() {
            StatusCode::OK
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => Ok(r),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if auth => {
                self.rejected.store(true, Ordering::Release);
                Err(Error::Credentials)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Denied),
            StatusCode::NOT_FOUND | StatusCode::GONE => Err(Error::Missing),
            StatusCode::CONFLICT => Err(Error::Changed),
            StatusCode::TOO_MANY_REQUESTS => {
                let t = now();
                let delay = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| {
                        s.parse::<u64>()
                            .ok()
                            .map(|n| t.saturating_add(n))
                            .or_else(|| {
                                httpdate::parse_http_date(s)
                                    .ok()?
                                    .duration_since(UNIX_EPOCH)
                                    .ok()
                                    .map(|d| d.as_secs())
                            })
                    })
                    .unwrap_or(t.saturating_add(60));
                self.retry_at
                    .fetch_max(delay.max(t.saturating_add(1)), Ordering::AcqRel);
                Err(Error::RateLimited)
            }
            StatusCode::BAD_REQUEST => Err(Error::Invalid),
            _ => Err(Error::Unavailable),
        }
    }
    async fn bytes(&self, mut r: reqwest::Response, max: usize) -> Result<Vec<u8>, Error> {
        if r.status() != StatusCode::OK {
            return Err(Error::Denied);
        }
        if r.content_length().is_some_and(|n| n > max as u64) {
            return Err(Error::Limit);
        }
        let mut data = Vec::new();
        while let Some(chunk) = r.chunk().await.map_err(|_| Error::Unavailable)? {
            if chunk.len() > max - data.len() {
                return Err(Error::Limit);
            }
            data.extend_from_slice(&chunk);
        }
        if self.refused() {
            return Err(Error::Credentials);
        }
        Ok(data)
    }
    async fn json<F, Fut>(&self, url: Url, check: &F) -> Result<(Value, Option<String>), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        let r = self.send(url, true, true, check).await?;
        let link = r
            .headers()
            .get("link")
            .map(|v| v.to_str().map(str::to_owned).map_err(|_| Error::Protocol))
            .transpose()?;
        let doc =
            serde_json::from_slice(&self.bytes(r, LIMIT).await?).map_err(|_| Error::Protocol)?;
        Ok((doc, link))
    }
    pub async fn verify_tenant<F, Fut>(&self, r: &Registration, check: &F) -> Result<(), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        let reply = self
            .send(
                url_path(&self.site, &["_edge", "tenant_info"])?,
                false,
                true,
                check,
            )
            .await?;
        let v: Value = serde_json::from_slice(&self.bytes(reply, 16 * 1024).await?)
            .map_err(|_| Error::Protocol)?;
        if v["cloudId"] != r.cloud_id {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub async fn verify_space<F, Fut>(&self, s: &Space, check: &F) -> Result<(), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        let (raw, _) = self
            .json(url_path(&self.api, &["api", "v2", "spaces", &s.id])?, check)
            .await?;
        if raw["id"] != s.id || raw["key"] != s.key || raw["status"] != "current" {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub async fn page<F, Fut>(
        &self,
        s: &Space,
        id: &str,
        body: bool,
        check: &F,
    ) -> Result<Value, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        let mut url = url_path(&self.api, &["api", "v2", "pages", id])?;
        if body {
            url.query_pairs_mut()
                .append_pair("body-format", "atlas_doc_format");
        }
        let (v, _) = self.json(url, check).await?;
        if v["id"] != id || v["spaceId"] != s.id || v["status"] != "current" {
            return Err(Error::Changed);
        }
        version(&v)?;
        Ok(v)
    }
    pub async fn verify_page<F, Fut>(&self, p: &Page, s: &Space, check: &F) -> Result<(), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        let raw = self.page(s, &p.id, false, check).await?;
        if version(&raw)? != p.version {
            return Err(Error::Changed);
        }
        Ok(())
    }
    fn continuation(&self, request: &Url, link: &str) -> Result<String, Error> {
        let u = request.join(link).map_err(|_| Error::Protocol)?;
        if (u.origin() != self.api.origin() && u.origin() != self.site.origin())
            || !u.username().is_empty()
            || u.password().is_some()
            || u.fragment().is_some()
        {
            return Err(Error::Protocol);
        }
        // The only reused part is the cursor. The next request rebuilds its endpoint, space and CQL locally.
        let mut cursors = u.query_pairs().filter(|(k, _)| k == "cursor");
        let cursor = cursors
            .next()
            .map(|(_, v)| v.into_owned())
            .ok_or(Error::Protocol)?;
        if cursors.next().is_some() || cursor.is_empty() || cursor.len() > 1024 {
            return Err(Error::Protocol);
        }
        Ok(cursor)
    }
    async fn list<F, Fut>(
        &self,
        mut u: Url,
        limit: u8,
        cursor: Option<&str>,
        check: &F,
    ) -> Result<(Vec<Value>, Option<String>), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        u.query_pairs_mut().append_pair("limit", &limit.to_string());
        if let Some(c) = cursor {
            u.query_pairs_mut().append_pair("cursor", c);
        }
        let (mut doc, headers) = self.json(u.clone(), check).await?;
        let rows = doc["results"].as_array_mut().ok_or(Error::Protocol)?;
        if rows.len() > usize::from(limit) {
            return Err(Error::Protocol);
        }
        let rows = std::mem::take(rows);
        let mut next = doc["_links"]
            .get("next")
            .filter(|v| !v.is_null())
            .map(|v| self.continuation(&u, v.as_str().ok_or(Error::Protocol)?))
            .transpose()?;
        if let Some(links) = headers {
            for item in links.split(',') {
                let mut parts = item.trim().split(';');
                let uri = parts.next().ok_or(Error::Protocol)?.trim();
                if parts.any(|s| matches!(s.trim(), "rel=\"next\"" | "rel=next")) {
                    let c = self.continuation(
                        &u,
                        uri.strip_prefix('<')
                            .and_then(|s| s.strip_suffix('>'))
                            .ok_or(Error::Protocol)?,
                    )?;
                    if next.as_ref().is_some_and(|v| v != &c) {
                        return Err(Error::Protocol);
                    }
                    next = Some(c);
                }
            }
        }
        if cursor.is_some() && next.as_deref() == cursor {
            return Err(Error::Protocol);
        }
        Ok((rows, next))
    }
    pub async fn search<F, Fut>(
        &self,
        s: &Space,
        query: &str,
        limit: u8,
        cursor: Option<&str>,
        check: &F,
    ) -> Result<(Vec<String>, Option<String>), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let mut cql = format!("type = page AND space = {}", quote(&s.key));
        if !query.trim().is_empty() {
            cql.push_str(&format!(" AND title ~ {}", quote(query)));
        }
        let mut url = url_path(&self.api, &["rest", "api", "search"])?;
        url.query_pairs_mut()
            .append_pair("cql", &cql)
            .append_pair("expand", "content.space");
        let (rows, next) = self.list(url, limit, cursor, check).await?;
        let mut ids = Vec::new();
        let mut seen = BTreeSet::new();
        for v in rows {
            let c = &v["content"];
            let n = field(c, "id")?;
            if c["type"] != "page" || !decimal(n) || !seen.insert(n.to_string()) {
                return Err(Error::Protocol);
            }
            let space = c.get("space").ok_or(Error::Protocol)?;
            if space["key"] != s.key
                || !space.get("id").is_some_and(|v| {
                    v == &json!(s.id) || v.as_u64().map(|n| n.to_string()).as_ref() == Some(&s.id)
                })
            {
                return Err(Error::Changed);
            }
            ids.push(n.to_owned());
        }
        Ok((ids, next))
    }
    pub async fn children<F, Fut>(
        &self,
        id: &str,
        limit: u8,
        cursor: Option<&str>,
        check: &F,
    ) -> Result<(Vec<Value>, Option<String>), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        self.list(
            url_path(&self.api, &["api", "v2", "pages", id, "direct-children"])?,
            limit,
            cursor,
            check,
        )
        .await
    }
    pub async fn attachments<F, Fut>(
        &self,
        id: &str,
        cursor: Option<&str>,
        check: &F,
    ) -> Result<(Vec<Value>, Option<String>), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        self.list(
            url_path(&self.api, &["api", "v2", "pages", id, "attachments"])?,
            25,
            cursor,
            check,
        )
        .await
    }
    pub(super) async fn verify_attachment<F, Fut>(
        &self,
        s: &Space,
        m: &Attachment,
        check: &F,
    ) -> Result<(), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        self.page(s, &m.page, false, check).await?;
        let (v, _) = self
            .json(
                url_path(&self.api, &["api", "v2", "attachments", &m.id])?,
                check,
            )
            .await?;
        if attachment(&v, s, &m.page)? != *m {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub async fn read_attachment<F, Fut>(
        &self,
        _r: &Registration,
        s: &Space,
        m: &Attachment,
        check: &F,
    ) -> Result<Vec<u8>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), Error>>,
    {
        self.verify_attachment(s, m, check).await?;
        let mut url = url_path(
            &self.api,
            &[
                "rest",
                "api",
                "content",
                &m.page,
                "child",
                "attachment",
                &m.id,
                "download",
            ],
        )?;
        url.query_pairs_mut()
            .append_pair("version", &m.version.to_string());
        let mut authenticated = true;
        for _ in 0..4 {
            let response = self.send(url.clone(), authenticated, false, check).await?;
            if response.status() == StatusCode::OK {
                let b = self.bytes(response, m.bytes as usize).await?;
                if b.len() as u64 != m.bytes {
                    return Err(Error::Changed);
                }
                self.verify_attachment(s, m, check).await?;
                return Ok(b);
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or(Error::Protocol)?;
            let next = url.join(location).map_err(|_| Error::Protocol)?;
            if !next.username().is_empty() || next.password().is_some() || next.fragment().is_some()
            {
                return Err(Error::Denied);
            }
            let same_api = next.origin() == self.api.origin()
                && next.path().starts_with(&format!(
                    "{}/download/attachments/{}/",
                    self.api.path(),
                    m.page
                ));
            if authenticated && same_api {
                authenticated = true;
            } else if self.media.contains(&origin(&next)) {
                authenticated = false;
            } else {
                return Err(Error::Denied);
            }
            url = next;
        }
        Err(Error::Limit)
    }
}
pub(super) fn attachment(v: &Value, s: &Space, page: &str) -> Result<Attachment, Error> {
    let id = field(v, "id")?;
    if !attachment_id(id) || v["pageId"] != page || v["status"] != "current" {
        return Err(Error::Changed);
    }
    let title = field(v, "title")?;
    if title.is_empty() || title.len() > 1024 || title.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    let file_id = field(v, "fileId")?;
    if file_id.is_empty() || file_id.len() > 255 || file_id.chars().any(char::is_control) {
        return Err(Error::Protocol);
    }
    Ok(Attachment {
        file_id: file_id.into(),
        id: id.into(),
        page: page.into(),
        space: s.id.clone(),
        title: title.into(),
        version: version(v)?,
        media_type: field(v, "mediaType")?.into(),
        bytes: v["fileSize"].as_u64().ok_or(Error::Protocol)?,
    })
}
