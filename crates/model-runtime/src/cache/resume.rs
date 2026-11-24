//! Crash recovery metadata for an incomplete download. The marker contains only hashes.
use super::*;
use std::io::Read;

pub(super) fn source(spec: &ArtifactSpec) -> Result<String, CacheError> {
    let uri = spec.url.as_ref().ok_or(CacheError::LocalFileRequired)?;
    orange_protocol::request_digest(
        "artifact-download-source.v1",
        &(
            uri.as_str(),
            &spec.allowed_hosts,
            &spec.digest,
            spec.bytes,
            &spec.magic,
        ),
    )
    .map_err(|_| CacheError::Invalid)
}
pub(super) fn representation(
    effective: &Url,
    etag: Option<&reqwest::header::HeaderValue>,
) -> String {
    let mut url = effective.clone();
    let tag = etag.map(|v| v.as_bytes()).unwrap_or_default();
    // Signed CDN query parameters may rotate independently of a strong representation tag.
    // Without that witness a changed effective URL requires a fresh transfer.
    if tag.len() >= 2
        && tag.first() == Some(&b'"')
        && tag.last() == Some(&b'"')
        && tag[1..tag.len() - 1]
            .iter()
            .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b) || *b >= 0x80)
    {
        url.set_query(None);
    }
    let mut hash = Sha256::new();
    hash.update(b"orange-remote-object-v1\0");
    hash.update(url.as_str().as_bytes());
    hash.update([0]);
    hash.update(tag);
    hex::encode(hash.finalize())
}
pub(super) fn prior(marker: &Path, source: &str) -> Result<Option<String>, CacheError> {
    let f = match private_file(marker, false) {
        Ok(f) => f,
        Err(CacheError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut data = Vec::with_capacity(130);
    f.take(130).read_to_end(&mut data)?;
    if data.len() != 129 {
        return Ok(None);
    }
    let Ok(raw) = std::str::from_utf8(&data) else {
        return Ok(None);
    };
    let Some((old, tag)) = raw.split_once('\n') else {
        return Ok(None);
    };
    Ok((old == source && orange_protocol::valid_digest(tag)).then(|| tag.to_owned()))
}
pub(super) fn publish(marker: &Path, source: &str, representation: &str) -> Result<(), CacheError> {
    let mut output = private_file(marker, true)?;
    output.set_len(0)?;
    write!(output, "{source}\n{representation}")?;
    output.sync_all()?;
    #[cfg(unix)]
    File::open(marker.parent().ok_or(CacheError::UnsafePath)?)?.sync_all()?;
    Ok(())
}
