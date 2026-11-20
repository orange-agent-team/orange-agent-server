//! CN-06 operations on one operator-registered bucket and one explicitly granted directory prefix.
use axum::http::{Method, Request, Response};
use opendal::{
    Buffer, HttpBody, HttpTransport, HttpTransporter, OperationContext, Operator, services::S3,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{net::IpAddr, time::Duration};
use tokio_stream::StreamExt;

pub const MAX_OBJECT: usize = 20 * 1024 * 1024;
const MAX_LIST: usize = 2 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid S3 source or object selector")]
    Invalid,
    #[error("source access denied")]
    Denied,
    #[error("object unavailable")]
    Missing,
    #[error("object version changed")]
    Changed,
    #[error("object or listing exceeds limits")]
    Limit,
    #[error("source rate limit reached")]
    Busy,
    #[error("source unavailable")]
    Unavailable,
}
impl From<opendal::Error> for Error {
    fn from(e: opendal::Error) -> Self {
        match e.kind() {
            opendal::ErrorKind::PermissionDenied => Self::Denied,
            opendal::ErrorKind::NotFound => Self::Missing,
            opendal::ErrorKind::ConditionNotMatch => Self::Changed,
            opendal::ErrorKind::RateLimited => Self::Busy,
            _ => Self::Unavailable,
        }
    }
}

fn relative(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 1024
        && !key.chars().any(char::is_control)
        && !key.contains('\\')
        && key.split('/').all(|s| {
            let decoded = percent_encoding::percent_decode_str(s).decode_utf8_lossy();
            !s.is_empty()
                && decoded != "."
                && decoded != ".."
                && !decoded.contains(['/', '\\'])
                && !decoded.chars().any(char::is_control)
        })
}
#[derive(Clone)]
struct Transport {
    rejected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    client: reqwest::Client,
    origin: reqwest::Url,
    bucket_path: String,
    prefix: String,
}
impl HttpTransport for Transport {
    async fn fetch(&self, req: Request<Buffer>) -> opendal::Result<Response<HttpBody>> {
        let blocked = || {
            opendal::Error::new(
                opendal::ErrorKind::PermissionDenied,
                "operation outside registered source",
            )
        };
        let unavailable = || {
            opendal::Error::new(
                opendal::ErrorKind::Unexpected,
                "invalid or oversized source response",
            )
        };
        let (parts, body) = req.into_parts();
        let url = reqwest::Url::parse(&parts.uri.to_string()).map_err(|_| blocked())?;
        if url.origin() != self.origin.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(parts.method, Method::HEAD | Method::GET)
            || !body.is_empty()
        {
            return Err(blocked());
        }
        if self.rejected.load(std::sync::atomic::Ordering::Acquire) {
            return Err(blocked());
        }
        let head = parts.method == Method::HEAD;
        let path = percent_encoding::percent_decode_str(url.path())
            .decode_utf8()
            .map_err(|_| blocked())?;
        let query: Vec<_> = url.query_pairs().collect();
        let is_list = query
            .iter()
            .any(|(name, value)| name == "list-type" && value == "2");
        let limit = if is_list {
            let prefixes: Vec<_> = query.iter().filter(|(k, _)| k == "prefix").collect();
            if head
                || path.trim_end_matches('/') != self.bucket_path
                || prefixes.len() != 1
                || prefixes[0].1 != self.prefix
            {
                return Err(blocked());
            }
            MAX_LIST
        } else {
            let object = path
                .strip_prefix(&self.bucket_path)
                .and_then(|s| s.strip_prefix('/'))
                .and_then(|s| s.strip_prefix(&self.prefix));
            if !object.is_some_and(relative) || query.iter().any(|(key, _)| key != "versionId") {
                return Err(blocked());
            }
            MAX_OBJECT
        };
        let mut reply = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if matches!(reply.status().as_u16(), 401 | 403) {
            self.rejected
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(blocked());
        }
        if reply.status().is_redirection() {
            return Err(blocked());
        }
        let mut response = Response::builder()
            .status(reply.status())
            .version(reply.version());
        *response.headers_mut().ok_or_else(unavailable)? = reply.headers().clone();
        let mut bytes = Vec::new();
        if !head {
            if reply.content_length().is_some_and(|n| n > limit as u64) {
                return Err(unavailable());
            }
            while let Some(chunk) = reply.chunk().await.map_err(|_| unavailable())? {
                if chunk.len() > limit - bytes.len() {
                    return Err(unavailable());
                }
                bytes.extend_from_slice(&chunk);
            }
        }
        let len = bytes.len() as u64;
        response
            .body(HttpBody::new(
                tokio_stream::once(Ok(Buffer::from(bytes))),
                Some(len),
            ))
            .map_err(|_| unavailable())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub address: IpAddr,
    pub port: u16,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
}
#[derive(PartialEq, Eq, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub key: String,
    pub etag: String,
    pub version: Option<String>,
    pub size_bytes: String,
}
#[derive(Serialize)]
pub struct Listing {
    pub keys: Vec<String>,
    pub next_after: Option<String>,
}
pub struct Content {
    pub head: Head,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Clone)]
pub struct Source {
    rejected: std::sync::Arc<std::sync::atomic::AtomicBool>,
    op: Operator,
}
impl Registration {
    pub fn open_development(&self, access: &str, secret: &str) -> Result<Source, Error> {
        if !self.address.is_loopback()
            || self.port == 0
            || !(3..=63).contains(&self.bucket.len())
            || !self
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || !self.bucket.as_bytes()[0].is_ascii_alphanumeric()
            || !self.bucket.as_bytes()[self.bucket.len() - 1].is_ascii_alphanumeric()
            || self.region.is_empty()
            || self.region.len() > 64
            || !self
                .region
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !relative(&self.prefix)
            || access.is_empty()
            || secret.is_empty()
        {
            return Err(Error::Invalid);
        }
        let addr = std::net::SocketAddr::new(self.address, self.port);
        let endpoint = format!("http://{addr}");
        let backend = S3::default()
            .endpoint(&endpoint)
            .region(&self.region)
            .bucket(&self.bucket)
            .root(&self.prefix)
            .access_key_id(access)
            .secret_access_key(secret)
            .disable_config_load()
            .disable_ec2_metadata();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| Error::Unavailable)?;
        let rejected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Transport {
            rejected: rejected.clone(),
            bucket_path: format!("/{}", self.bucket),
            prefix: format!("{}/", self.prefix),
            client,
            origin: reqwest::Url::parse(&endpoint).map_err(|_| Error::Invalid)?,
        };
        let ctx = OperationContext::new().with_http_transport(HttpTransporter::new(transport));
        Ok(Source {
            rejected,
            op: Operator::new(backend)?.with_context(ctx),
        })
    }
}
impl Source {
    pub fn authorization_refused(&self) -> bool {
        self.rejected.load(std::sync::atomic::Ordering::Acquire)
    }
    pub async fn list(&self, after: Option<&str>, limit: usize) -> Result<Listing, Error> {
        if !(1..=100).contains(&limit) || after.is_some_and(|s| !relative(s)) {
            return Err(Error::Invalid);
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut options = self.op.lister_with("").recursive(true).limit(limit + 1);
            if let Some(key) = after {
                options = options.start_after(key);
            }
            let mut listing = options.await?;
            let mut keys: Vec<String> = Vec::new();
            while let Some(entry) = listing.try_next().await? {
                let key = entry.path();
                if entry.metadata().is_dir() {
                    continue;
                }
                if !relative(key)
                    || keys
                        .last()
                        .map(String::as_str)
                        .or(after)
                        .is_some_and(|previous| key <= previous)
                {
                    return Err(Error::Changed);
                }
                if keys.len() == limit {
                    let next_after = keys.last().cloned();
                    return Ok(Listing { keys, next_after });
                }
                keys.push(key.to_owned());
            }
            Ok(Listing {
                keys,
                next_after: None,
            })
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
    pub async fn head(&self, key: &str) -> Result<Head, Error> {
        if !relative(key) {
            return Err(Error::Invalid);
        }
        let m = self.op.stat(key).await?;
        if !m.is_file() {
            return Err(Error::Missing);
        }
        let etag = m
            .etag()
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .ok_or(Error::Changed)?;
        Ok(Head {
            key: key.to_owned(),
            etag: etag.into(),
            version: m.version().map(str::to_owned),
            size_bytes: m.content_length().to_string(),
        })
    }
    pub async fn read(&self, expected: &Head) -> Result<Content, Error> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let current = self.head(&expected.key).await?;
            if current != *expected {
                return Err(Error::Changed);
            }
            let size = current
                .size_bytes
                .parse::<usize>()
                .map_err(|_| Error::Limit)?;
            if size > MAX_OBJECT {
                return Err(Error::Limit);
            }
            let mut read = self.op.read_with(&current.key).if_match(&current.etag);
            if let Some(version) = &current.version {
                read = read.version(version);
            }
            let data = read.await?;
            if data.len() != size || self.head(&current.key).await? != current {
                return Err(Error::Changed);
            }
            let bytes = data.to_vec();
            let sha256 = hex::encode(Sha256::digest(&bytes));
            Ok(Content {
                head: current,
                bytes,
                sha256,
            })
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}
