use super::*;
use serde_json::json;

#[derive(Clone, Copy)]
pub enum Kind {
    Issue,
    Change,
}
impl Source {
    fn collection(&self, r: &Repository, kind: Kind) -> Result<&'static str, Error> {
        match (kind, self.provider) {
            (Kind::Issue, _) if r.issues => Ok("issues"),
            (Kind::Change, Provider::Github) if r.changes => Ok("pulls"),
            (Kind::Change, Provider::Gitlab) if r.changes => Ok("merge_requests"),
            _ => Err(Error::Denied),
        }
    }
    fn item(
        &self,
        r: &Repository,
        kind: Kind,
        v: &Value,
        detail: bool,
    ) -> Result<Option<Value>, Error> {
        let issue = matches!(kind, Kind::Issue);
        if self.provider == Provider::Github && issue && v.get("pull_request").is_some() {
            return Ok(None);
        }
        let number = match self.provider {
            Provider::Github => {
                if issue {
                    let expected = self.url(r, &[])?;
                    if !v["repository_url"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(expected.as_str()))
                    {
                        return Err(Error::Changed);
                    }
                } else if v["base"]["repo"]["id"].as_u64() != Some(r.id) {
                    return Err(Error::Changed);
                }
                &v["number"]
            }
            Provider::Gitlab => {
                let project = if issue {
                    "project_id"
                } else {
                    "target_project_id"
                };
                if v[project].as_u64() != Some(r.id) {
                    return Err(Error::Changed);
                }
                &v["iid"]
            }
        };
        let number = number.as_u64().filter(|n| *n > 0).ok_or(Error::Protocol)?;
        let mut result = json!({"repository_id":r.id.to_string(),"number":number.to_string(),"id":v["id"].as_u64().ok_or(Error::Protocol)?.to_string(),
            "title":v["title"].as_str().ok_or(Error::Protocol)?,"state":v["state"].as_str().ok_or(Error::Protocol)?,"updated_at":v["updated_at"].as_str().ok_or(Error::Protocol)?});
        if detail {
            let key = if self.provider == Provider::Github {
                "body"
            } else {
                "description"
            };
            let body = v.get(key).ok_or(Error::Protocol)?;
            if !body.is_null() && !body.is_string() {
                return Err(Error::Protocol);
            }
            result["body"] = body.clone();
        }
        Ok(Some(result))
    }
    pub async fn list_items(
        &self,
        allowed: &BTreeSet<u64>,
        id: u64,
        kind: Kind,
        page: u32,
        limit: usize,
    ) -> Result<Page, Error> {
        check_page(page, limit)?;
        let repo = self.repo(allowed, id)?;
        let mut url = self.url(repo, &[self.collection(repo, kind)?])?;
        url.query_pairs_mut()
            .append_pair("page", &page.to_string())
            .append_pair("per_page", &limit.to_string())
            .append_pair("state", "all");
        tokio::time::timeout(Duration::from_secs(30), async {
            self.identity(repo).await?;
            let response = self.get(url, false, METADATA_LIMIT, false).await?;
            let v = response.json()?;
            let items = v.as_array().ok_or(Error::Protocol)?;
            if items.len() > limit {
                return Err(Error::Protocol);
            }
            let data = items
                .iter()
                .filter_map(|v| self.item(repo, kind, v, false).transpose())
                .collect::<Result<Vec<_>, _>>()?;
            self.identity(repo).await?;
            Ok(Page {
                data,
                next_page: response.next(page)?,
            })
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
    pub async fn read_item(
        &self,
        allowed: &BTreeSet<u64>,
        id: u64,
        kind: Kind,
        number: u64,
    ) -> Result<Value, Error> {
        if number == 0 {
            return Err(Error::Invalid);
        }
        let repo = self.repo(allowed, id)?;
        let url = self.url(repo, &[self.collection(repo, kind)?, &number.to_string()])?;
        tokio::time::timeout(Duration::from_secs(30), async {
            self.identity(repo).await?;
            let v = self.get(url, false, METADATA_LIMIT, false).await?.json()?;
            let item = self.item(repo, kind, &v, true)?.ok_or(Error::Missing)?;
            let field = if self.provider == Provider::Github {
                "number"
            } else {
                "iid"
            };
            if v[field].as_u64() != Some(number) {
                return Err(Error::Changed);
            }
            self.identity(repo).await?;
            Ok(item)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}
