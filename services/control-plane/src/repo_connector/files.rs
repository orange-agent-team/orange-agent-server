use super::*;
use sha2::{Digest, Sha256};

fn oid(bytes: &[u8], expected: &str) -> Result<(), Error> {
    let header = format!("blob {}\0", bytes.len());
    let computed = match expected.len() {
        40 => hex::encode(
            sha1::Sha1::new()
                .chain_update(header.as_bytes())
                .chain_update(bytes)
                .finalize(),
        ),
        64 => hex::encode(
            Sha256::new()
                .chain_update(header.as_bytes())
                .chain_update(bytes)
                .finalize(),
        ),
        _ => return Err(Error::Protocol),
    };
    if computed != expected {
        return Err(Error::Changed);
    }
    Ok(())
}
fn field_id(value: &Value, key: &str) -> Result<String, Error> {
    value[key]
        .as_str()
        .filter(|s| commit_id(s))
        .map(str::to_owned)
        .ok_or(Error::Protocol)
}
impl Source {
    async fn github_file(&self, r: &Repository, revision: &str, path: &str) -> Result<Blob, Error> {
        let mut url = self.url(r, &["commits", revision])?;
        url.query_pairs_mut().append_pair("per_page", "1");
        let commit = self.get(url, false, METADATA_LIMIT, false).await?.json()?;
        let pinned = field_id(&commit, "sha")?;
        let mut tree = field_id(&commit["commit"]["tree"], "sha")?;
        let mut parts = path.split('/').peekable();
        while let Some(component) = parts.next() {
            let list = self
                .get(
                    self.url(r, &["git", "trees", &tree])?,
                    false,
                    METADATA_LIMIT,
                    false,
                )
                .await?
                .json()?;
            if field_id(&list, "sha")? != tree || list["truncated"].as_bool() != Some(false) {
                return Err(Error::Limit);
            }
            let mut matched = list["tree"]
                .as_array()
                .ok_or(Error::Protocol)?
                .iter()
                .filter(|x| x["path"] == component);
            let entry = matched.next().ok_or(Error::Missing)?;
            if matched.next().is_some() {
                return Err(Error::Protocol);
            }
            tree = field_id(entry, "sha")?;
            if parts.peek().is_some() {
                if entry["type"] != "tree" || entry["mode"] != "040000" {
                    return Err(Error::Denied);
                }
            } else {
                if entry["type"] != "blob"
                    || !matches!(entry["mode"].as_str(), Some("100644" | "100755"))
                {
                    return Err(Error::Denied);
                }
                let size = entry["size"].as_u64().ok_or(Error::Protocol)?;
                if size > FILE_LIMIT as u64 {
                    return Err(Error::Limit);
                }
                let data = self
                    .get(
                        self.url(r, &["git", "blobs", &tree])?,
                        true,
                        FILE_LIMIT,
                        false,
                    )
                    .await?
                    .bytes;
                if data.len() as u64 != size {
                    return Err(Error::Changed);
                }
                return verified(path, pinned, tree, data);
            }
        }
        Err(Error::Missing)
    }
    async fn gitlab_file(&self, r: &Repository, revision: &str, path: &str) -> Result<Blob, Error> {
        let commit = self
            .get(
                self.url(r, &["repository", "commits", revision])?,
                false,
                METADATA_LIMIT,
                false,
            )
            .await?
            .json()?;
        let pinned = field_id(&commit, "id")?;
        let mut head = self.url(r, &["repository", "files", path])?;
        head.query_pairs_mut().append_pair("ref", &pinned);
        let response = self.get(head, false, METADATA_LIMIT, true).await?;
        let header = |k: &str| {
            response
                .headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .ok_or(Error::Protocol)
        };
        if header("x-gitlab-commit-id")? != pinned || header("x-gitlab-file-path")? != path {
            return Err(Error::Changed);
        }
        let blob_id = header("x-gitlab-blob-id")?.to_owned();
        if !commit_id(&blob_id) {
            return Err(Error::Protocol);
        }
        let sha = header("x-gitlab-content-sha256")?;
        let size = header("x-gitlab-size")?
            .parse::<usize>()
            .map_err(|_| Error::Protocol)?;
        if size > FILE_LIMIT {
            return Err(Error::Limit);
        }
        let parent = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        let mut page = 1;
        loop {
            let mut url = self.url(r, &["repository", "tree"])?;
            url.query_pairs_mut()
                .append_pair("ref", &pinned)
                .append_pair("path", parent)
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let reply = self.get(url, false, METADATA_LIMIT, false).await?;
            let nodes = reply.json()?;
            let nodes = nodes.as_array().ok_or(Error::Protocol)?;
            if nodes.len() > 100 {
                return Err(Error::Protocol);
            }
            if let Some(node) = nodes.iter().find(|n| n["path"] == path) {
                if node["id"] != blob_id
                    || node["type"] != "blob"
                    || !matches!(node["mode"].as_str(), Some("100644" | "100755"))
                {
                    return Err(Error::Changed);
                }
                break;
            }
            page = reply.next(page)?.ok_or(Error::Missing)?;
            if page > 32 {
                return Err(Error::Limit);
            }
        }
        let data = self
            .get(
                self.url(r, &["repository", "blobs", &blob_id, "raw"])?,
                true,
                FILE_LIMIT,
                false,
            )
            .await?
            .bytes;
        if data.len() != size {
            return Err(Error::Changed);
        }
        let result = verified(path, pinned, blob_id, data)?;
        if result.sha256 != sha {
            return Err(Error::Changed);
        }
        Ok(result)
    }
    pub(crate) fn file_scope(
        &self,
        allowed: &BTreeSet<u64>,
        id: u64,
        revision: &str,
        path: &str,
    ) -> Result<&Repository, Error> {
        let repo = self.repo(allowed, id)?;
        if !clean_path(path)
            || path.split('/').count() > 32
            || !clean_path(revision)
            || revision.len() > 255
        {
            return Err(Error::Invalid);
        }
        if !repo.file_prefixes.iter().any(|prefix| {
            prefix.is_empty()
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        }) {
            return Err(Error::Denied);
        }
        Ok(repo)
    }
    pub async fn read_file(
        &self,
        allowed: &BTreeSet<u64>,
        id: u64,
        revision: &str,
        path: &str,
    ) -> Result<Blob, Error> {
        let repo = self.file_scope(allowed, id, revision, path)?;
        tokio::time::timeout(Duration::from_secs(30), async {
            self.identity(repo).await?;
            let result = match self.provider {
                Provider::Github => self.github_file(repo, revision, path).await?,
                Provider::Gitlab => self.gitlab_file(repo, revision, path).await?,
            };
            if commit_id(revision) && result.commit != revision {
                return Err(Error::Changed);
            }
            self.identity(repo).await?;
            Ok(result)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}
fn verified(path: &str, commit: String, id: String, bytes: Vec<u8>) -> Result<Blob, Error> {
    oid(&bytes, &id)?;
    Ok(Blob {
        path: path.to_owned(),
        commit,
        oid: id,
        sha256: hex::encode(Sha256::digest(&bytes)),
        bytes,
    })
}
