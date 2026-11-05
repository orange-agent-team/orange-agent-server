//! Private application cache for approved artifacts. Integrity does not itself approve a model.
//! No eviction of existing models; quota exhaustion is explicit. Transfers never log signed URLs.
use reqwest::{Client, Url};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Seek, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncReadExt;
mod import;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("artifact import cancelled before publication")]
    Cancelled,
    #[error("invalid artifact descriptor")]
    Invalid,
    #[error("cache path must be a private regular-file namespace")]
    UnsafePath,
    #[error("cache lock unavailable")]
    LockUnavailable,
    #[error("model cache quota exceeded")]
    Quota,
    #[error("artifact transfer timed out")]
    Timeout,
    #[error("artifact connection failed")]
    Connection,
    #[error("artifact transport interrupted")]
    Transport,
    #[error("artifact redirect rejected")]
    Redirect,
    #[error("artifact HTTP status {0}")]
    Http(u16),
    #[error("invalid artifact range or length response")]
    Protocol,
    #[error("artifact transfer incomplete")]
    Incomplete,
    #[error("artifact content exceeded pinned size")]
    TooLarge,
    #[error("artifact digest or magic mismatch; never activated")]
    Integrity,
    #[error("cache filesystem operation failed")]
    Io(#[from] io::Error),
}
impl CacheError {
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Connection
                | Self::Transport
                | Self::Incomplete
                | Self::Integrity
                | Self::Http(408 | 429 | 500..=599)
        )
    }
}
fn network(error: reqwest::Error) -> CacheError {
    if error.is_redirect() {
        CacheError::Redirect
    } else if error.is_timeout() {
        CacheError::Timeout
    } else if error.is_connect() {
        CacheError::Connection
    } else {
        CacheError::Transport
    }
}
#[derive(Clone)]
pub struct ArtifactSpec {
    url: Url,
    digest: String,
    bytes: u64,
    allowed_hosts: BTreeSet<String>,
    magic: Vec<u8>,
}
impl ArtifactSpec {
    /// Descriptor and host set must come from an approved registry/configuration, never model arguments.
    pub fn new(
        url: &str,
        digest: &str,
        bytes: u64,
        hosts: &[&str],
        magic: &[u8],
    ) -> Result<Self, CacheError> {
        let url = Url::parse(url).map_err(|_| CacheError::Invalid)?;
        let allowed_hosts = hosts.iter().map(|h| h.to_string()).collect::<BTreeSet<_>>();
        if !orange_protocol::valid_digest(digest)
            || bytes == 0
            || bytes > (1_u64 << 40)
            || magic.len() > 64
            || magic.len() as u64 > bytes
            || allowed_hosts.is_empty()
            || !allowed_url(&url, &allowed_hosts)
        {
            return Err(CacheError::Invalid);
        }
        Ok(Self {
            url,
            digest: digest.into(),
            bytes,
            allowed_hosts,
            magic: magic.to_vec(),
        })
    }
}
fn allowed_url(url: &Url, hosts: &BTreeSet<String>) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.port_or_known_default() == Some(443)
        && url.host_str().is_some_and(|h| hosts.contains(h))
}
pub struct VerifiedArtifact {
    path: PathBuf,
    digest: String,
    bytes: u64,
    cached: bool,
}
impl VerifiedArtifact {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn sha256(&self) -> &str {
        &self.digest
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn is_cached(&self) -> bool {
        self.cached
    }
}
pub struct ModelCache {
    root: PathBuf,
    quota: u64,
    retry_delay: Duration,
}
impl ModelCache {
    pub fn new(root: &Path, quota: u64) -> Result<Self, CacheError> {
        if quota == 0 {
            return Err(CacheError::Invalid);
        }
        if !root.exists() {
            fs::create_dir_all(root)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
            }
        }
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(CacheError::UnsafePath);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(CacheError::UnsafePath);
            }
        }
        Ok(Self {
            root: root.canonicalize()?,
            quota,
            retry_delay: Duration::from_secs(1),
        })
    }
    pub async fn ensure(&self, spec: &ArtifactSpec) -> Result<VerifiedArtifact, CacheError> {
        let hosts = spec.allowed_hosts.clone();
        let client = Client::builder()
            .https_only(true)
            .user_agent(concat!(
                "orange-agent-artifact-cache/",
                env!("CARGO_PKG_VERSION")
            ))
            .referer(false)
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 5 || !allowed_url(attempt.url(), &hosts) {
                    // Never print signed query strings or URL credentials.
                    eprintln!(
                        "artifact redirect denied scheme={} host={} hops={}",
                        attempt.url().scheme(),
                        attempt.url().host_str().unwrap_or("none"),
                        attempt.previous().len()
                    );
                    attempt.error("artifact redirect denied")
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(network)?;
        self.ensure_with(spec, &HttpTransport { client }).await
    }
    async fn ensure_with<T: Transport>(
        &self,
        spec: &ArtifactSpec,
        transport: &T,
    ) -> Result<VerifiedArtifact, CacheError> {
        let ready = self.root.join(format!("{}.gguf", spec.digest));
        let partial = self.root.join(format!("{}.part", spec.digest));
        if exists(&ready)? {
            verify(&ready, spec).await?;
            return Ok(artifact(&ready, spec, true));
        }
        // Serialize quota reservations for this cache. Ready immutable files remain readable concurrently.
        let lock = cache_lease(&self.root.join(".model-cache-lock"))?;
        if exists(&ready)? {
            verify(&ready, spec).await?;
            return Ok(artifact(&ready, spec, true));
        }
        let previous = if exists(&partial)? {
            private_file(&partial, false)?.metadata()?.len()
        } else {
            0
        };
        if previous > spec.bytes {
            return Err(CacheError::TooLarge);
        }
        if self
            .used_bytes()?
            .checked_add(spec.bytes - previous)
            .is_none_or(|used| used > self.quota)
        {
            return Err(CacheError::Quota);
        }
        for attempt in 0..3_u32 {
            let progress = if exists(&partial)? {
                private_file(&partial, false)?.metadata()?.len()
            } else {
                0
            };
            eprintln!(
                "artifact attempt={} retained_bytes={} expected_bytes={}",
                attempt + 1,
                progress,
                spec.bytes
            );
            let result = if progress == spec.bytes {
                verify(&partial, spec).await
            } else {
                transfer_once(transport, spec, &partial, lock.clone()).await
            };
            match result {
                Ok(()) => {
                    fs::rename(&partial, &ready)?;
                    #[cfg(unix)]
                    {
                        File::open(&self.root)?.sync_all()?;
                    }
                    return Ok(artifact(&ready, spec, false));
                }
                Err(error) if error.retryable() && attempt < 2 => {
                    // A corrupt prefix is discarded only in our owned staging file, never an active model.
                    if matches!(error, CacheError::Integrity) {
                        let file = private_file(&partial, true)?;
                        file.set_len(0)?;
                        file.sync_all()?;
                    }
                    eprintln!("artifact retry reason={error}");
                    tokio::time::sleep(self.retry_delay * (1 << attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(CacheError::Incomplete)
    }
    fn used_bytes(&self) -> Result<u64, CacheError> {
        let mut total = 0_u64;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some((digest, extension)) = name.rsplit_once('.') else {
                continue;
            };
            if orange_protocol::valid_digest(digest)
                && ["gguf", "part", "import"].contains(&extension)
            {
                let file = private_file(&entry.path(), false)?;
                total = total
                    .checked_add(file.metadata()?.len())
                    .ok_or(CacheError::Quota)?;
            }
        }
        Ok(total)
    }
}
fn artifact(path: &Path, spec: &ArtifactSpec, cached: bool) -> VerifiedArtifact {
    VerifiedArtifact {
        path: path.into(),
        digest: spec.digest.clone(),
        bytes: spec.bytes,
        cached,
    }
}
struct CacheLease {
    file: File,
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        // Only the final real cache writer owns this Drop. Explicit unlock
        // prevents an incidental dup/fork descriptor from extending a completed
        // cache lease. Model execution leases intentionally have other semantics.
        let _ = self.file.unlock();
    }
}
fn cache_lease(path: &Path) -> Result<Arc<CacheLease>, CacheError> {
    let file = private_file(path, true)?;
    file.try_lock().map_err(|_| CacheError::LockUnavailable)?;
    Ok(Arc::new(CacheLease { file }))
}
fn exists(path: &Path) -> Result<bool, CacheError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CacheError::Io(error)),
    }
}
fn private_file(path: &Path, create: bool) -> Result<File, CacheError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CacheError::UnsafePath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(CacheError::UnsafePath);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(CacheError::UnsafePath);
        }
    }
    Ok(file)
}
async fn verify(path: &Path, spec: &ArtifactSpec) -> Result<(), CacheError> {
    let original = private_file(path, false)?;
    if original.metadata()?.len() != spec.bytes {
        return Err(CacheError::Incomplete);
    }
    let mut file = tokio::fs::File::from_std(original);
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut prefix = Vec::new();
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if prefix.len() < spec.magic.len() {
            let needed = (spec.magic.len() - prefix.len()).min(count);
            prefix.extend_from_slice(&buffer[..needed]);
        }
        hash.update(&buffer[..count]);
    }
    if prefix != spec.magic || hex::encode(hash.finalize()) != spec.digest {
        return Err(CacheError::Integrity);
    }
    Ok(())
}
struct DownloadResponse<B> {
    status: u16,
    length: Option<u64>,
    range: Option<String>,
    body: B,
}
trait DownloadBody {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, CacheError>;
}
trait Transport {
    type Body: DownloadBody;
    async fn open(
        &self,
        url: &Url,
        offset: u64,
    ) -> Result<DownloadResponse<Self::Body>, CacheError>;
}
struct HttpTransport {
    client: Client,
}
struct HttpBody(reqwest::Response);
impl DownloadBody for HttpBody {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, CacheError> {
        self.0
            .chunk()
            .await
            .map(|value| value.map(|b| b.to_vec()))
            .map_err(network)
    }
}
impl Transport for HttpTransport {
    type Body = HttpBody;
    async fn open(&self, url: &Url, offset: u64) -> Result<DownloadResponse<HttpBody>, CacheError> {
        let mut request = self
            .client
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::CACHE_CONTROL, "no-cache");
        if offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        let mut response = request.send().await.map_err(network)?;
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            let host = response.url().host_str().unwrap_or("none").to_owned();
            let sample = tokio::time::timeout(Duration::from_secs(2), async {
                let mut bytes = Vec::new();
                while bytes.len() < 4096 {
                    let Some(chunk) = response.chunk().await.map_err(network)? else {
                        break;
                    };
                    bytes.extend_from_slice(&chunk[..chunk.len().min(4096 - bytes.len())]);
                }
                Ok::<_, CacheError>(bytes)
            })
            .await;
            let code = if let Ok(Ok(bytes)) = sample {
                let text = String::from_utf8_lossy(&bytes);
                let candidate = text
                    .split_once("<Code>")
                    .and_then(|(_, tail)| tail.split_once("</Code>"))
                    .map(|(code, _)| code);
                match candidate {
                    Some("AccessDenied") => "AccessDenied",
                    Some("SignatureDoesNotMatch") => "SignatureDoesNotMatch",
                    Some("ExpiredToken") => "ExpiredToken",
                    Some("SecurityTokenExpired") => "SecurityTokenExpired",
                    Some("RequestTimeTooSkewed") => "RequestTimeTooSkewed",
                    _ => "Unclassified",
                }
            } else {
                "Unavailable"
            };
            eprintln!("artifact HTTP403 host={host} public_error_code={code}");
            return Err(CacheError::Http(403));
        }
        Ok(DownloadResponse {
            status: response.status().as_u16(),
            length: response.content_length(),
            range: response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            body: HttpBody(response),
        })
    }
}
fn range_start(value: &str, expected: u64) -> Option<u64> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    if total.parse::<u64>().ok()? != expected || end.checked_add(1)? != expected || start > end {
        return None;
    }
    Some(start)
}
async fn transfer_once<T: Transport>(
    transport: &T,
    spec: &ArtifactSpec,
    path: &Path,
    lock: Arc<CacheLease>,
) -> Result<(), CacheError> {
    let original = private_file(path, true)?;
    let mut length = original.metadata()?.len();
    if length > spec.bytes {
        return Err(CacheError::TooLarge);
    }
    let file = Arc::new(Mutex::new(original));
    let mut response = transport.open(&spec.url, length).await?;
    match response.status {
        200 => {
            if response.range.is_some() {
                return Err(CacheError::Protocol);
            }
            disk_io(file.clone(), lock.clone(), |file| {
                file.set_len(0)?;
                file.seek(io::SeekFrom::Start(0))?;
                Ok(())
            })
            .await?;
            length = 0;
        }
        206 => {
            if response
                .range
                .as_deref()
                .and_then(|v| range_start(v, spec.bytes))
                != Some(length)
            {
                return Err(CacheError::Protocol);
            }
            disk_io(file.clone(), lock.clone(), move |file| {
                file.seek(io::SeekFrom::Start(length))?;
                Ok(())
            })
            .await?;
        }
        code => return Err(CacheError::Http(code)),
    }
    if response.length.is_some_and(|n| n != spec.bytes - length) {
        return Err(CacheError::Protocol);
    }
    let mut last_report = length;
    loop {
        let chunk = match response.body.next().await {
            Ok(value) => value,
            Err(error) => {
                disk_io(file.clone(), lock.clone(), |file| file.sync_all()).await?;
                return Err(error);
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if chunk.len() as u64 > spec.bytes - length {
            disk_io(file.clone(), lock.clone(), |file| file.sync_all()).await?;
            return Err(CacheError::TooLarge);
        }
        let count = chunk.len() as u64;
        disk_io(file.clone(), lock.clone(), move |file| {
            file.write_all(&chunk)
        })
        .await?;
        length += count;
        if length - last_report >= 32 * 1024 * 1024 {
            eprintln!(
                "artifact progress_bytes={length} expected_bytes={}",
                spec.bytes
            );
            last_report = length;
        }
    }
    disk_io(file.clone(), lock.clone(), |file| file.sync_all()).await?;
    if length != spec.bytes {
        return Err(CacheError::Incomplete);
    }
    drop(file);
    verify(path, spec).await
}

// A dropped async caller must not release the cache lease while a blocking write still owns the file.
async fn disk_io<R: Send + 'static>(
    file: Arc<Mutex<File>>,
    lock: Arc<CacheLease>,
    operation: impl FnOnce(&mut File) -> io::Result<R> + Send + 'static,
) -> Result<R, CacheError> {
    tokio::task::spawn_blocking(move || {
        let _cache_lease = lock;
        let mut file = file
            .lock()
            .map_err(|_| io::Error::other("cache file lock poisoned"))?;
        operation(&mut file)
    })
    .await
    .map_err(|_| CacheError::Io(io::Error::other("cache I/O worker failed")))?
    .map_err(CacheError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};
    #[test]
    fn completed_cache_lease_does_not_follow_unrelated_duplicate_descriptors() {
        let directory = tempfile::tempdir().expect("directory");
        let _cache = cache(directory.path(), 1024);
        let path = directory.path().join(".model-cache-lock");
        let lease = cache_lease(&path).expect("lease");
        // Models descriptor inheritance/duplication independently of Rust lease
        // owners. This handle never writes cache data and must not extend it.
        let duplicate = lease.file.try_clone().expect("duplicate descriptor");
        let writer = lease.clone();
        drop(lease);
        let contender = private_file(&path, true).expect("contender");
        assert!(
            contender.try_lock().is_err(),
            "actual writer still owns the lease"
        );
        drop(writer);
        assert!(
            contender.try_lock().is_ok(),
            "no actual writer remains; incidental duplicate must not hold cache lease"
        );
        drop(duplicate);
    }
    struct Script {
        offset: u64,
        status: u16,
        length: Option<u64>,
        range: Option<String>,
        chunks: Vec<Result<Vec<u8>, CacheError>>,
    }
    struct Fake {
        scripts: Mutex<VecDeque<Script>>,
        offsets: Mutex<Vec<u64>>,
    }
    struct FakeBody(VecDeque<Result<Vec<u8>, CacheError>>);
    impl DownloadBody for FakeBody {
        async fn next(&mut self) -> Result<Option<Vec<u8>>, CacheError> {
            self.0.pop_front().transpose()
        }
    }
    impl Transport for Fake {
        type Body = FakeBody;
        async fn open(
            &self,
            _url: &Url,
            offset: u64,
        ) -> Result<DownloadResponse<FakeBody>, CacheError> {
            self.offsets.lock().expect("offset lock").push(offset);
            let script = self
                .scripts
                .lock()
                .expect("script lock")
                .pop_front()
                .expect("unexpected transport request");
            assert_eq!(offset, script.offset);
            Ok(DownloadResponse {
                status: script.status,
                length: script.length,
                range: script.range,
                body: FakeBody(script.chunks.into()),
            })
        }
    }
    fn fake(scripts: Vec<Script>) -> Fake {
        Fake {
            scripts: Mutex::new(scripts.into()),
            offsets: Mutex::new(Vec::new()),
        }
    }
    fn spec(data: &[u8]) -> ArtifactSpec {
        ArtifactSpec::new(
            "https://model.example/pinned.gguf",
            &hex::encode(Sha256::digest(data)),
            data.len() as u64,
            &["model.example", "cdn.example"],
            b"GGUF",
        )
        .expect("descriptor")
    }
    fn cache(path: &Path, quota: u64) -> ModelCache {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("private fixture directory");
        }
        let mut cache = ModelCache::new(path, quota).expect("private cache");
        cache.retry_delay = Duration::ZERO;
        cache
    }
    fn full(data: &[u8]) -> Script {
        Script {
            offset: 0,
            status: 200,
            length: Some(data.len() as u64),
            range: None,
            chunks: vec![Ok(data.to_vec())],
        }
    }
    #[tokio::test]
    async fn interrupted_transfer_resumes_exact_range_and_only_verified_file_is_activated() {
        let directory = tempfile::tempdir().expect("directory");
        let data = b"GGUF model fixture";
        let spec = spec(data);
        let cache = cache(directory.path(), 1024);
        let transport = fake(vec![
            Script {
                offset: 0,
                status: 200,
                length: Some(data.len() as u64),
                range: None,
                chunks: vec![Ok(data[..5].to_vec()), Err(CacheError::Transport)],
            },
            Script {
                offset: 5,
                status: 206,
                length: Some(data.len() as u64 - 5),
                range: Some(format!("bytes 5-{}/{}", data.len() - 1, data.len())),
                chunks: vec![Ok(data[5..].to_vec())],
            },
        ]);
        let artifact = cache.ensure_with(&spec, &transport).await.expect("resume");
        assert_eq!(fs::read(artifact.path()).expect("file"), data);
        assert!(!artifact.is_cached());
        assert_eq!(*transport.offsets.lock().expect("offsets"), vec![0, 5]);
        assert!(
            !directory
                .path()
                .join(format!("{}.part", spec.digest))
                .exists()
        );
        let no_network = fake(vec![]);
        assert!(
            cache
                .ensure_with(&spec, &no_network)
                .await
                .expect("verified cache hit")
                .is_cached()
        );
    }
    #[tokio::test]
    async fn completed_staging_file_after_crash_is_verified_without_redownload() {
        let directory = tempfile::tempdir().expect("directory");
        let data = b"GGUF complete";
        let spec = spec(data);
        let cache = cache(directory.path(), 1024);
        fs::write(directory.path().join(format!("{}.part", spec.digest)), data)
            .expect("crash fixture");
        let result = cache
            .ensure_with(&spec, &fake(vec![]))
            .await
            .expect("recover verified staging");
        assert_eq!(fs::read(result.path()).expect("ready"), data);
    }
    #[tokio::test]
    async fn range_mismatch_and_oversize_never_activate_or_append_wrong_content() {
        let directory = tempfile::tempdir().expect("directory");
        let data = b"GGUF fixture";
        let spec = spec(data);
        let cache = cache(directory.path(), 1024);
        let partial = directory.path().join(format!("{}.part", spec.digest));
        fs::write(&partial, &data[..4]).expect("prefix");
        let bad = fake(vec![Script {
            offset: 4,
            status: 206,
            length: Some(data.len() as u64 - 4),
            range: Some(format!("bytes 0-{}/{}", data.len() - 1, data.len())),
            chunks: vec![Ok(data.to_vec())],
        }]);
        assert!(matches!(
            cache.ensure_with(&spec, &bad).await,
            Err(CacheError::Protocol)
        ));
        assert_eq!(fs::read(&partial).expect("unchanged"), data[..4]);
        fs::remove_file(&partial).expect("owned fixture reset");
        let huge = fake(vec![Script {
            offset: 0,
            status: 200,
            length: None,
            range: None,
            chunks: vec![Ok(vec![0; data.len() + 1])],
        }]);
        assert!(matches!(
            cache.ensure_with(&spec, &huge).await,
            Err(CacheError::TooLarge)
        ));
        assert!(
            !directory
                .path()
                .join(format!("{}.gguf", spec.digest))
                .exists()
        );
    }
    #[tokio::test]
    async fn corrupt_bytes_get_only_three_attempts_and_are_never_published() {
        let directory = tempfile::tempdir().expect("directory");
        let data = b"GGUF correct";
        let spec = spec(data);
        let cache = cache(directory.path(), 1024);
        let wrong = vec![b'x'; data.len()];
        let source = fake(vec![full(&wrong), full(&wrong), full(&wrong)]);
        assert!(matches!(
            cache.ensure_with(&spec, &source).await,
            Err(CacheError::Integrity)
        ));
        assert_eq!(source.offsets.lock().expect("attempts").len(), 3);
        assert!(
            !directory
                .path()
                .join(format!("{}.gguf", spec.digest))
                .exists()
        );
    }
    #[tokio::test]
    async fn quota_and_real_filesystem_lock_fail_before_network() {
        let directory = tempfile::tempdir().expect("directory");
        let data = b"GGUF quota";
        let spec = spec(data);
        let small = cache(directory.path(), 1);
        assert!(matches!(
            small.ensure_with(&spec, &fake(vec![])).await,
            Err(CacheError::Quota)
        ));
        let guard =
            private_file(&directory.path().join(".model-cache-lock"), true).expect("lock file");
        guard.try_lock().expect("hold lock");
        let cache = cache(directory.path(), 1024);
        assert!(matches!(
            cache.ensure_with(&spec, &fake(vec![])).await,
            Err(CacheError::LockUnavailable)
        ));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn staging_symlink_cannot_overwrite_an_external_file() {
        let directory = tempfile::tempdir().expect("directory");
        let outside = tempfile::tempdir().expect("outside fixture");
        let target = outside.path().join("keep");
        fs::write(&target, b"unchanged").expect("target");
        let spec = spec(b"GGUF safe");
        let cache = cache(directory.path(), 1024);
        std::os::unix::fs::symlink(
            &target,
            directory.path().join(format!("{}.part", spec.digest)),
        )
        .expect("symlink fixture");
        assert!(cache.ensure_with(&spec, &fake(vec![])).await.is_err());
        assert_eq!(fs::read(target).expect("outside"), b"unchanged");
    }
    #[test]
    fn redirects_and_descriptors_reject_userinfo_http_and_unapproved_hosts() {
        let hosts = BTreeSet::from(["model.example".into(), "cdn.example".into()]);
        for value in [
            "http://model.example/file",
            "https://model.example.evil/file",
            "https://127.0.0.1/file",
            "https://user@model.example/file",
            "https://model.example:8443/file",
            "https://model.example/file#fragment",
        ] {
            assert!(!allowed_url(
                &Url::parse(value).expect("fixture URL"),
                &hosts
            ));
        }
        assert!(allowed_url(
            &Url::parse("https://cdn.example/file?signature=fixture").expect("URL"),
            &hosts
        ));
        assert!(
            ArtifactSpec::new(
                "https://model.example/file",
                "../../bad",
                5,
                &["model.example"],
                b"GGUF"
            )
            .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn publicly_readable_cache_directory_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
            .expect("public fixture mode");
        assert!(matches!(
            ModelCache::new(directory.path(), 1024),
            Err(CacheError::UnsafePath)
        ));
    }
    #[tokio::test]
    async fn cancelling_async_caller_does_not_release_lock_while_disk_write_is_in_flight() {
        let directory = tempfile::tempdir().expect("directory");
        let _cache = cache(directory.path(), 1024);
        let lock_path = directory.path().join(".model-cache-lock");
        let lock = cache_lease(&lock_path).expect("lease");
        let target = directory.path().join("io-fixture");
        let file = Arc::new(Mutex::new(private_file(&target, true).expect("file")));
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(disk_io(file, lock, move |file| {
            started
                .send(())
                .map_err(|_| io::Error::other("fixture receiver gone"))?;
            release_rx
                .recv()
                .map_err(|_| io::Error::other("fixture release gone"))?;
            file.write_all(b"owned write")?;
            file.sync_all()
        }));
        started_rx.await.expect("write started");
        task.abort();
        let _ = task.await;
        let contender = private_file(&lock_path, true).expect("contender");
        assert!(contender.try_lock().is_err());
        release.send(()).expect("allow actual write to finish");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if contender.try_lock().is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("lease released only after write");
        assert_eq!(fs::read(target).expect("persisted bytes"), b"owned write");
    }
}
