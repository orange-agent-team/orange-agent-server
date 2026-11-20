//! Read-only repository connector sources. Destinations are selected by the operator, never by a tool.
mod files;
mod objects;
pub use objects::Kind;
use reqwest::{
    Client, StatusCode, Url,
    header::{ACCEPT, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const METADATA_LIMIT: usize = 2 * 1024 * 1024;
const FILE_LIMIT: usize = 20 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Github,
    Gitlab,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid repository request")]
    Invalid,
    #[error("scope not granted")]
    Denied,
    #[error("resource not found")]
    Missing,
    #[error("registered repository identity or object changed")]
    Changed,
    #[error("source credentials no longer valid")]
    Credentials,
    #[error("source is rate limited")]
    RateLimited,
    #[error("source request or payload exceeds the limit")]
    Limit,
    #[error("source response violates the selected protocol")]
    Protocol,
    #[error("repository source unavailable")]
    Unavailable,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub id: u64,
    pub path: String,
    /// Each entry is a directory relative to the repo root; the empty entry explicitly grants the whole repository.
    pub file_prefixes: Vec<String>,
    pub issues: bool,
    pub changes: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub provider: Provider,
    pub repositories: Vec<Repository>,
}
pub struct Source {
    provider: Provider,
    origin: Url,
    client: Client,
    repos: BTreeMap<u64, Repository>,
    credential: Option<HeaderValue>,
    refused: AtomicBool,
    retry_at: std::sync::atomic::AtomicU64,
}
pub struct Blob {
    pub path: String,
    pub commit: String,
    pub oid: String,
    pub sha256: String,
    pub bytes: Vec<u8>,
}
#[derive(Serialize)]
pub struct Page {
    pub data: Vec<Value>,
    pub next_page: Option<u32>,
}
struct Reply {
    headers: HeaderMap,
    bytes: Vec<u8>,
}
impl Reply {
    fn json(&self) -> Result<Value, Error> {
        serde_json::from_slice(&self.bytes).map_err(|_| Error::Protocol)
    }
    fn next(&self, page: u32) -> Result<Option<u32>, Error> {
        let expected = page.checked_add(1).ok_or(Error::Limit)?;
        let continuation = self
            .headers
            .get("link")
            .and_then(|s| s.to_str().ok())
            .is_some_and(|s| {
                s.split(',').any(|part| {
                    part.split(';')
                        .skip(1)
                        .any(|attr| attr.trim() == "rel=\"next\"")
                })
            });
        if let Some(value) = self.headers.get("x-next-page") {
            let s = value.to_str().map_err(|_| Error::Protocol)?;
            if !s.is_empty() {
                if s.parse::<u32>().ok() != Some(expected) {
                    return Err(Error::Protocol);
                }
                return Ok(Some(expected));
            }
        }
        Ok(continuation.then_some(expected))
    }
}
fn clean_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}
fn commit_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn check_page(page: u32, limit: usize) -> Result<(), Error> {
    if page == 0 || page > 10000 || !(1..=100).contains(&limit) {
        return Err(Error::Invalid);
    }
    Ok(())
}
impl Registration {
    /// A token, if supplied, must originate in the host's credential store.
    pub fn open(&self, token: Option<&str>) -> Result<Source, Error> {
        let origin = match self.provider {
            Provider::Github => "https://api.github.com",
            Provider::Gitlab => "https://gitlab.com/api/v4",
        };
        self.create(Url::parse(origin).map_err(|_| Error::Invalid)?, token)
    }
    fn create(&self, origin: Url, token: Option<&str>) -> Result<Source, Error> {
        if !(1..=100).contains(&self.repositories.len()) {
            return Err(Error::Invalid);
        }
        let mut repos = BTreeMap::new();
        let mut paths = BTreeSet::new();
        for r in &self.repositories {
            let parts: Vec<_> = r.path.split('/').collect();
            if r.id == 0
                || !clean_path(&r.path)
                || !(2..=16).contains(&parts.len())
                || self.provider == Provider::Github && parts.len() != 2
                || !r
                    .path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-._".contains(&b))
                || !paths.insert(r.path.to_ascii_lowercase())
                || r.file_prefixes.len() > 32
                || r.file_prefixes
                    .iter()
                    .any(|s| !s.is_empty() && !clean_path(s))
                || repos.insert(r.id, r.clone()).is_some()
            {
                return Err(Error::Invalid);
            }
        }
        let credential = token
            .map(|s| {
                if s.is_empty() || s.len() > 4096 || !s.bytes().all(|b| b.is_ascii_graphic()) {
                    return Err(Error::Invalid);
                }
                let mut v =
                    HeaderValue::from_str(&format!("Bearer {s}")).map_err(|_| Error::Invalid)?;
                v.set_sensitive(true);
                Ok(v)
            })
            .transpose()?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Orange-Agent/0.1 repository-reader")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Error::Unavailable)?;
        Ok(Source {
            provider: self.provider,
            origin,
            client,
            repos,
            credential,
            refused: AtomicBool::new(false),
            retry_at: std::sync::atomic::AtomicU64::new(0),
        })
    }
}
impl Source {
    fn repo<'a>(&'a self, allowed: &BTreeSet<u64>, id: u64) -> Result<&'a Repository, Error> {
        if !allowed.contains(&id) {
            return Err(Error::Denied);
        }
        self.repos.get(&id).ok_or(Error::Denied)
    }
    fn url(&self, r: &Repository, tail: &[&str]) -> Result<Url, Error> {
        let mut u = self.origin.clone();
        let mut p = u.path_segments_mut().map_err(|_| Error::Invalid)?;
        p.pop_if_empty();
        match self.provider {
            Provider::Github => {
                p.push("repos").extend(r.path.split('/'));
            }
            Provider::Gitlab => {
                p.push("projects").push(&r.id.to_string());
            }
        }
        p.extend(tail.iter().copied());
        drop(p);
        Ok(u)
    }
    pub fn authorization_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }
    fn unix_time() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
    fn throttle(&self, headers: &HeaderMap) -> Error {
        let now = Self::unix_time();
        let delay = headers
            .get("retry-after")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| {
                s.parse::<u64>()
                    .ok()
                    .map(|seconds| now.saturating_add(seconds))
                    .or_else(|| {
                        httpdate::parse_http_date(s)
                            .ok()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs())
                    })
            });
        let reset = headers
            .get("x-ratelimit-reset")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let until = delay
            .into_iter()
            .chain(reset)
            .max()
            .unwrap_or(now.saturating_add(60))
            .max(now.saturating_add(1));
        self.retry_at.fetch_max(until, Ordering::AcqRel);
        Error::RateLimited
    }
    async fn get(&self, url: Url, raw: bool, maximum: usize, head: bool) -> Result<Reply, Error> {
        if self.refused.load(Ordering::Acquire) {
            return Err(Error::Credentials);
        }
        if self.retry_at.load(Ordering::Acquire) > Self::unix_time() {
            return Err(Error::RateLimited);
        }
        if url.origin() != self.origin.origin() {
            return Err(Error::Denied);
        }
        let mut q = if head {
            self.client.head(url)
        } else {
            self.client.get(url)
        };
        q = q.header(
            ACCEPT,
            if raw && self.provider == Provider::Github {
                "application/vnd.github.raw+json"
            } else if raw {
                "application/octet-stream"
            } else {
                "application/json"
            },
        );
        if self.provider == Provider::Github {
            q = q.header("X-GitHub-Api-Version", "2022-11-28");
        }
        if let Some(token) = &self.credential {
            q = q.header(reqwest::header::AUTHORIZATION, token);
        }
        let mut response = q.send().await.map_err(|_| Error::Unavailable)?;
        let headers = response.headers().clone();
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED => {
                self.refused.store(true, Ordering::Release);
                return Err(Error::Credentials);
            }
            StatusCode::TOO_MANY_REQUESTS => return Err(self.throttle(&headers)),
            StatusCode::FORBIDDEN
                if headers.contains_key("retry-after")
                    || headers
                        .get("x-ratelimit-remaining")
                        .is_some_and(|v| v == "0") =>
            {
                return Err(self.throttle(&headers));
            }
            StatusCode::FORBIDDEN if self.credential.is_some() => {
                self.refused.store(true, Ordering::Release);
                return Err(Error::Credentials);
            }
            StatusCode::FORBIDDEN => return Err(Error::Denied),
            StatusCode::NOT_FOUND => return Err(Error::Missing),
            status if status.is_redirection() => return Err(Error::Changed),
            _ => return Err(Error::Unavailable),
        }
        let mut bytes = Vec::new();
        if !head {
            if response
                .content_length()
                .is_some_and(|n| n > maximum as u64)
            {
                return Err(Error::Limit);
            }
            while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
                if chunk.len() > maximum - bytes.len() {
                    return Err(Error::Limit);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
        if self.refused.load(Ordering::Acquire) {
            return Err(Error::Credentials);
        }
        Ok(Reply { headers, bytes })
    }
    async fn identity(&self, r: &Repository) -> Result<Value, Error> {
        let value = self
            .get(self.url(r, &[])?, false, METADATA_LIMIT, false)
            .await?
            .json()?;
        let field = if self.provider == Provider::Github {
            "full_name"
        } else {
            "path_with_namespace"
        };
        if value["id"].as_u64() != Some(r.id)
            || !value[field]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&r.path))
        {
            return Err(Error::Changed);
        }
        Ok(
            serde_json::json!({"id":r.id.to_string(),"path":r.path,"default_branch":value["default_branch"].as_str().ok_or(Error::Protocol)?}),
        )
    }
    pub async fn list_repositories(
        &self,
        allowed: &BTreeSet<u64>,
        page: u32,
        limit: usize,
    ) -> Result<Page, Error> {
        check_page(page, limit)?;
        if !allowed.iter().all(|id| self.repos.contains_key(id)) {
            return Err(Error::Denied);
        }
        let offset = (page as usize - 1) * limit;
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut data = Vec::new();
            for id in allowed.iter().skip(offset).take(limit) {
                data.push(self.identity(self.repo(allowed, *id)?).await?);
            }
            Ok(Page {
                data,
                next_page: (offset + limit < allowed.len()).then_some(page + 1),
            })
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, extract::Request, http::Response, routing::any};
    use std::sync::{Arc, atomic::AtomicUsize};
    fn fixture_repo() -> Registration {
        Registration {
            provider: Provider::Github,
            repositories: vec![Repository {
                id: 17,
                path: "owner/repo".into(),
                file_prefixes: vec!["notes".into()],
                issues: true,
                changes: false,
            }],
        }
    }
    async fn fixture(code: StatusCode) -> (Source, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("private listener");
        let origin = Url::parse(&format!("http://{}", socket.local_addr().expect("address")))
            .expect("origin");
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let repo_url = origin.join("/repos/owner/repo").expect("fixture URL");
        let server = tokio::spawn(async move {
            let app=Router::new().fallback(any(move |request:Request|{
                let counter=count.clone();let url=repo_url.clone();
                async move{
                    assert_eq!(request.method(),"GET");
                    assert_eq!(request.headers()["authorization"],"Bearer fixture-secret");
                    let n=counter.fetch_add(1,Ordering::SeqCst);
                    let (status,body)=if code!=StatusCode::OK{(code,serde_json::json!({"code":"source_error"}))}
                        else if request.uri().path().ends_with("issues/1"){
                            (code,serde_json::json!({"repository_url":url.as_str(),"id":7,"number":1,"title":"item","state":"open","updated_at":"2020-01-01T00:00:00Z","body":"sensitive"}))
                        }else{(code,serde_json::json!({"id":if n==0{17}else{18},"full_name":"owner/repo","default_branch":"main"}))};
                    Response::builder().status(status).header("retry-after","30").body(Body::from(body.to_string())).expect("fixture")
                }
            }));
            axum::serve(socket, app).await.expect("serve");
        });
        (
            fixture_repo()
                .create(origin, Some("fixture-secret"))
                .expect("local fixture"),
            hits,
            server,
        )
    }
    #[tokio::test]
    async fn a_disallowed_scope_never_contacts_the_source_and_changed_identity_discards_content() {
        let (s, hits, server) = fixture(StatusCode::OK).await;
        let scope = BTreeSet::from([17]);
        assert!(matches!(
            s.read_file(&scope, 17, "main", "secret.txt").await,
            Err(Error::Denied)
        ));
        assert!(matches!(
            s.list_items(&scope, 17, Kind::Change, 1, 2).await,
            Err(Error::Denied)
        ));
        assert!(matches!(
            s.read_item(&BTreeSet::new(), 17, Kind::Issue, 1).await,
            Err(Error::Denied)
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(matches!(
            s.read_item(&scope, 17, Kind::Issue, 1).await,
            Err(Error::Changed)
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        server.abort();
    }
    #[tokio::test]
    async fn credential_refusals_and_rate_limits_suppress_the_next_outbound_call() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::TOO_MANY_REQUESTS] {
            let (s, hits, server) = fixture(status).await;
            for _ in 0..2 {
                let err = s.list_repositories(&BTreeSet::from([17]), 1, 1).await;
                match status {
                    StatusCode::UNAUTHORIZED => assert!(matches!(err, Err(Error::Credentials))),
                    _ => assert!(matches!(err, Err(Error::RateLimited))),
                }
            }
            assert_eq!(
                s.authorization_refused(),
                status == StatusCode::UNAUTHORIZED
            );
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }
}
