//! Keep provider capabilities in the control process. Only validated GenerateRequest data
//! enters the model runner; no URL, cookie, node key, redirect or ambient proxy is used.
use super::*;
use reqwest::{Client, Url};

#[derive(Clone)]
pub(super) struct DirectInput {
    origin: String,
    client: Client,
}
fn now() -> Result<i64, ControlError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|t| i64::try_from(t.as_secs()).ok())
        .ok_or(ControlError::Stale)
}
impl DirectInput {
    pub(super) fn configured() -> Result<Option<Self>, ControlError> {
        let Some(value) = std::env::var_os("ORANGE_WORKER_ARTIFACT_DOWNLOAD_ORIGIN") else {
            return Ok(None);
        };
        let origin = value.into_string().map_err(|_| ControlError::Invalid)?;
        let url = Url::parse(&origin).map_err(|_| ControlError::Invalid)?;
        let local = url.host_str().is_some_and(|h| {
            h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if url.origin().ascii_serialization() != origin
            || !(url.scheme() == "https" || url.scheme() == "http" && local)
        {
            return Err(ControlError::Invalid);
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| ControlError::Unavailable)?;
        Ok(Some(Self { origin, client }))
    }
    fn checked_url(
        &self,
        grant: &worker::InputDownloadReply,
    ) -> Result<(Url, String), ControlError> {
        let url = Url::parse(&grant.get_url).map_err(|_| ControlError::Protocol)?;
        if url.origin().ascii_serialization() != self.origin
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !url
                .path_segments()
                .and_then(Iterator::last)
                .and_then(|p| p.strip_prefix("published-"))
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        {
            return Err(ControlError::Protocol);
        }
        let field = |key: &str| {
            let mut pairs = url.query_pairs().filter(|(k, _)| k == key);
            let first = pairs.next().ok_or(ControlError::Protocol)?.1.into_owned();
            if pairs.next().is_some() {
                return Err(ControlError::Protocol);
            }
            Ok(first)
        };
        let version = field("versionId")?;
        if version.is_empty()
            || version == "null"
            || version.len() > 1024
            || !version.bytes().all(|b| b.is_ascii_graphic())
            || field("X-Amz-Algorithm")? != "AWS4-HMAC-SHA256"
            || !orange_protocol::valid_digest(&field("X-Amz-Signature")?)
            || !field("X-Amz-Expires")?
                .parse::<u64>()
                .is_ok_and(|n| (1..=60).contains(&n))
        {
            return Err(ControlError::Protocol);
        }
        Ok((url, version))
    }
    pub(super) async fn read(
        &self,
        grant: &worker::InputDownloadReply,
        input: &worker::InputArtifact,
        deadline: Instant,
    ) -> Result<Vec<u8>, ControlError> {
        if input.size_bytes == 0
            || input.size_bytes > 256 * 1024
            || input.media_type != "application/json"
        {
            return Err(ControlError::Protocol);
        }
        let (url, version) = self.checked_url(grant)?;
        let remaining = grant.expires_unix - now()?;
        if !(1..=60).contains(&remaining) {
            return Err(ControlError::Stale);
        }
        let until = deadline.min(Instant::now() + Duration::from_secs(remaining as u64));
        let read = async {
            let mut response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|_| ControlError::Unavailable)?;
            if response.status() != reqwest::StatusCode::OK
                || response
                    .headers()
                    .get("x-amz-version-id")
                    .and_then(|v| v.to_str().ok())
                    != Some(version.as_str())
                || response
                    .content_length()
                    .is_some_and(|len| len != input.size_bytes)
            {
                return Err(ControlError::Protocol);
            }
            let mut bytes = Vec::with_capacity(input.size_bytes as usize);
            while let Some(part) = response
                .chunk()
                .await
                .map_err(|_| ControlError::Unavailable)?
            {
                if part.len() > input.size_bytes as usize - bytes.len() {
                    return Err(ControlError::Protocol);
                }
                bytes.extend_from_slice(&part);
            }
            if bytes.len() as u64 != input.size_bytes || now()? >= grant.expires_unix {
                return Err(ControlError::Stale);
            }
            Ok(bytes)
        };
        tokio::time::timeout_at(until.into(), read)
            .await
            .map_err(|_| ControlError::Stale)?
    }
}
impl ControlConnection {
    pub(super) async fn direct_input_rpc(
        &mut self,
        request: &worker::InputDownloadRequest,
        deadline: Instant,
    ) -> Result<worker::InputDownloadReply, ControlError> {
        let reply = tokio::time::timeout_at(
            deadline.into(),
            self.input_client.authorize_input_download(request.clone()),
        )
        .await
        .map_err(|_| ControlError::Stale)?
        .map_err(status)?
        .into_inner();
        if reply.protocol_version != 1
            || reply.session_id != self.identity.session_id
            || reply.command_id != request.command_id
            || request
                .lease
                .as_ref()
                .is_none_or(|l| l.attempt_id != reply.attempt_id)
            || reply.artifact != request.artifact
            || reply.verify_only != request.verify_only
            || reply.get_url.is_empty() != request.verify_only
        {
            return Err(ControlError::Protocol);
        }
        let ttl = reply.expires_unix - now()?;
        if !(1..=60).contains(&ttl) || Instant::now() >= deadline {
            return Err(ControlError::Stale);
        }
        Ok(reply)
    }
}
