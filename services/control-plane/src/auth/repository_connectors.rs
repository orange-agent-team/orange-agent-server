//! Personal Run grants for remote repository providers. Session state is rechecked after every source read.
use super::connector_access::{
    capacity, check_source, db, denied, disable_source, disabled, guard, key, missing,
};
use super::*;
use crate::{
    auth::users::UserSessionGuard,
    repo_connector::{Blob, Error, Kind, Provider, registry::Registered},
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeSet;

fn provider(name: &str) -> Result<Provider, UserApiError> {
    Provider::named(name).ok_or_else(missing)
}
fn decimal(value: &str) -> Result<u64, UserApiError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == value)
        .ok_or(AuthError::Invalid.into())
}
fn src(
    i: &UserIdentity,
    g: &UserSessionGuard,
    name: &str,
    f: Provider,
) -> Result<Arc<Registered>, UserApiError> {
    i.repository_sources
        .0
        .get(name)
        .filter(|s| s.registration.provider == f && s.owner == g.view().personal_owner_id)
        .cloned()
        .ok_or_else(missing)
}
async fn opted(g: &mut UserSessionGuard, run: &str) -> Result<bool, UserApiError> {
    sqlx::query_scalar("SELECT EXISTS(SELECT FROM cloud_external_source_opt_ins WHERE run_id=$1)")
        .bind(run)
        .fetch_one(&mut **g.transaction())
        .await
        .map_err(db)
}
fn conflict() -> UserApiError {
    UserApiError(StatusCode::CONFLICT, "REQUEST_ALREADY_CONSUMED")
}
fn source_error(e: Error) -> UserApiError {
    match e {
        Error::Changed => UserApiError(StatusCode::CONFLICT, "REPOSITORY_CHANGED"),
        Error::Credentials => UserApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "SOURCE_REGISTRATION_DISABLED",
        ),
        Error::Limit => UserApiError(StatusCode::PAYLOAD_TOO_LARGE, "SOURCE_LIMIT"),
        Error::RateLimited => UserApiError(StatusCode::TOO_MANY_REQUESTS, "SOURCE_RATE_LIMIT"),
        Error::Denied => denied(),
        Error::Missing => missing(),
        Error::Protocol | Error::Unavailable => AuthError::Unavailable.into(),
        Error::Invalid => AuthError::Invalid.into(),
    }
}
async fn live(
    i: &UserIdentity,
    g: &mut UserSessionGuard,
    run: &str,
    f: Provider,
    id: &str,
) -> Result<(Arc<Registered>, BTreeSet<u64>), UserApiError> {
    let row=sqlx::query("SELECT source_id,source_digest,repository_ids FROM repository_source_grants c JOIN cloud_external_source_opt_ins USING(run_id) WHERE run_id=$1 AND provider=$2 AND grant_id=$3 AND expires_at>clock_timestamp() AND NOT EXISTS(SELECT FROM repository_source_revocations r WHERE r.grant_id=c.grant_id)").bind(run).bind(f.name()).bind(id).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(denied)?;
    let s = src(i, g, row.get("source_id"), f)?;
    if s.digest != row.get::<String, _>("source_digest") {
        return Err(denied());
    }
    check_source(g, &s.digest).await?;
    let ids = row
        .get::<Vec<String>, _>("repository_ids")
        .iter()
        .map(|id| decimal(id))
        .collect::<Result<_, _>>()?;
    Ok((s, ids))
}
pub(super) async fn list(
    State(i): State<Arc<UserIdentity>>,
    Path((run, name)): Path<(String, String)>,
    h: HeaderMap,
) -> Result<Json<Value>, UserApiError> {
    let f = provider(&name)?;
    let (mut g, readable) = guard(&i, &h, &run, false, false).await?;
    let egress = opted(&mut g, &run).await?;
    let refused = disabled(&mut g).await?;
    let sources:Vec<_>=i.repository_sources.0.iter().filter(|(_,s)|s.owner==g.view().personal_owner_id&&s.registration.provider==f&&!refused.contains(&s.digest)).map(|(id,s)|{
        let resources:Vec<_>=s.registration.repositories.iter().map(|r|json!({"repository_id":r.id.to_string(),"path":r.path,"file_prefixes":r.file_prefixes,"issues":r.issues,"changes":r.changes})).collect();
        json!({"source_id":id,"source_digest":s.digest,"repositories":resources})
    }).collect();
    let grants:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('grant_id',g.grant_id,'source_id',g.source_id,'source_digest',g.source_digest,'repository_ids',g.repository_ids,'expires_at',g.expires_at,'revoked',v.grant_id IS NOT NULL) FROM repository_source_grants g LEFT JOIN repository_source_revocations v USING(grant_id) WHERE run_id=$1 AND provider=$2 ORDER BY expires_at DESC,grant_id LIMIT 100").bind(run).bind(name).fetch_all(&mut **g.transaction()).await.map_err(db)?;
    g.finish().await?;
    Ok(Json(
        json!({"sources":sources,"recent_grants":grants,"run_readable":readable,"external_access_selected":egress}),
    ))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consent {
    source_id: String,
    source_digest: String,
    repository_ids: Vec<String>,
    duration_seconds: u32,
}
pub(super) async fn grant(
    State(i): State<Arc<UserIdentity>>,
    Path((run, name)): Path<(String, String)>,
    h: HeaderMap,
    body: Result<Json<Consent>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let f = provider(&name)?;
    let Json(mut c) = body.map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&i, &h, &run, true, true).await?;
    if !opted(&mut g, &run).await? {
        return Err(UserApiError(
            StatusCode::FORBIDDEN,
            "EXTERNAL_CONNECTORS_DISABLED",
        ));
    }
    let s = src(&i, &g, &c.source_id, f)?;
    check_source(&mut g, &s.digest).await?;
    c.repository_ids.sort();
    if c.source_digest != s.digest
        || !(1..=3600).contains(&c.duration_seconds)
        || !(1..=100).contains(&c.repository_ids.len())
        || c.repository_ids.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(AuthError::Invalid.into());
    }
    for id in &c.repository_ids {
        let n = decimal(id)?;
        if !s.registration.repositories.iter().any(|r| r.id == n) {
            return Err(denied());
        }
    }
    let digest = orange_protocol::request_digest("repository.consent.v1", &(&run, f, &c))
        .map_err(|_| AuthError::Invalid)?;
    let req = key(&h)?;
    let grant = super::external_grants::Consent {
        run: &run,
        provider: f.name(),
        source: &c.source_id,
        source_digest: &s.digest,
        roots: &c.repository_ids,
        seconds: c.duration_seconds,
        key: req,
        digest: &digest,
    }
    .issue(&mut g)
    .await?;
    g.finish().await?;
    Ok(Json(json!({"grant_id":grant})))
}
pub(super) async fn revoke(
    State(i): State<Arc<UserIdentity>>,
    Path((run, name, id)): Path<(String, String, String)>,
    h: HeaderMap,
) -> Result<StatusCode, UserApiError> {
    let f = provider(&name)?;
    let (mut g, _) = guard(&i, &h, &run, true, false).await?;
    super::external_grants::revoke(&mut g, &run, f.name(), &id).await?;
    g.finish().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Action {
    ListRepositories {
        page: u32,
        limit: u16,
    },
    ListProjects {
        page: u32,
        limit: u16,
    },
    ReadFile {
        repository_id: String,
        revision: String,
        path: String,
    },
    ListIssues {
        repository_id: String,
        page: u32,
        limit: u16,
    },
    ReadIssue {
        repository_id: String,
        number: String,
    },
    ListPullRequests {
        repository_id: String,
        page: u32,
        limit: u16,
    },
    ReadPullRequest {
        repository_id: String,
        number: String,
    },
    ListMergeRequests {
        repository_id: String,
        page: u32,
        limit: u16,
    },
    ReadMergeRequest {
        repository_id: String,
        number: String,
    },
}
impl Action {
    fn authorize(&self, s: &Registered, allowed: &BTreeSet<u64>) -> Result<(), UserApiError> {
        use Action::*;
        let f = s.registration.provider;
        let wrong = || UserApiError(StatusCode::BAD_REQUEST, "WRONG_PROVIDER_ACTION");
        match self {
            ListRepositories { .. } | ListPullRequests { .. } | ReadPullRequest { .. }
                if f != Provider::Github =>
            {
                return Err(wrong());
            }
            ListProjects { .. } | ListMergeRequests { .. } | ReadMergeRequest { .. }
                if f != Provider::Gitlab =>
            {
                return Err(wrong());
            }
            ListRepositories { page, limit } | ListProjects { page, limit } => {
                crate::repo_connector::check_page(*page, usize::from(*limit))
            }
            ReadFile {
                repository_id,
                revision,
                path,
            } => s
                .reader
                .file_scope(allowed, decimal(repository_id)?, revision, path)
                .map(|_| ()),
            ListIssues {
                repository_id,
                page,
                limit,
            }
            | ListPullRequests {
                repository_id,
                page,
                limit,
            }
            | ListMergeRequests {
                repository_id,
                page,
                limit,
            } => {
                crate::repo_connector::check_page(*page, usize::from(*limit))
                    .map_err(source_error)?;
                s.reader.item_scope(
                    allowed,
                    decimal(repository_id)?,
                    if matches!(self, ListIssues { .. }) {
                        Kind::Issue
                    } else {
                        Kind::Change
                    },
                )
            }
            ReadIssue {
                repository_id,
                number,
            }
            | ReadPullRequest {
                repository_id,
                number,
            }
            | ReadMergeRequest {
                repository_id,
                number,
            } => {
                decimal(number)?;
                s.reader.item_scope(
                    allowed,
                    decimal(repository_id)?,
                    if matches!(self, ReadIssue { .. }) {
                        Kind::Issue
                    } else {
                        Kind::Change
                    },
                )
            }
        }
        .map_err(source_error)
    }
}
enum Data {
    Metadata(Value),
    File(u64, Blob),
}
async fn execute(s: &Registered, ids: &BTreeSet<u64>, a: Action) -> Result<Data, UserApiError> {
    let f = s.registration.provider;
    let invalid = || UserApiError(StatusCode::BAD_REQUEST, "WRONG_PROVIDER_ACTION");
    use Action::*;
    let value = match a {
        ListRepositories { .. } if f != Provider::Github => return Err(invalid()),
        ListProjects { .. } if f != Provider::Gitlab => return Err(invalid()),
        ListRepositories { page, limit } | ListProjects { page, limit } => json!(
            s.reader
                .list_repositories(ids, page, limit.into())
                .await
                .map_err(source_error)?
        ),
        ListIssues {
            repository_id,
            page,
            limit,
        } => json!(
            s.reader
                .list_items(
                    ids,
                    decimal(&repository_id)?,
                    Kind::Issue,
                    page,
                    limit.into()
                )
                .await
                .map_err(source_error)?
        ),
        ReadIssue {
            repository_id,
            number,
        } => s
            .reader
            .read_item(
                ids,
                decimal(&repository_id)?,
                Kind::Issue,
                decimal(&number)?,
            )
            .await
            .map_err(source_error)?,
        ListPullRequests { .. } | ReadPullRequest { .. } if f != Provider::Github => {
            return Err(invalid());
        }
        ListMergeRequests { .. } | ReadMergeRequest { .. } if f != Provider::Gitlab => {
            return Err(invalid());
        }
        ListPullRequests {
            repository_id,
            page,
            limit,
        }
        | ListMergeRequests {
            repository_id,
            page,
            limit,
        } => json!(
            s.reader
                .list_items(
                    ids,
                    decimal(&repository_id)?,
                    Kind::Change,
                    page,
                    limit.into()
                )
                .await
                .map_err(source_error)?
        ),
        ReadPullRequest {
            repository_id,
            number,
        }
        | ReadMergeRequest {
            repository_id,
            number,
        } => s
            .reader
            .read_item(
                ids,
                decimal(&repository_id)?,
                Kind::Change,
                decimal(&number)?,
            )
            .await
            .map_err(source_error)?,
        ReadFile {
            repository_id,
            path,
            revision,
        } => {
            let n = decimal(&repository_id)?;
            return Ok(Data::File(
                n,
                s.reader
                    .read_file(ids, n, &revision, &path)
                    .await
                    .map_err(source_error)?,
            ));
        }
    };
    Ok(Data::Metadata(value))
}
fn receipt(row: &sqlx::postgres::PgRow) -> Value {
    json!({"kind":"source_backed_artifact","artifact_id":row.get::<String,_>("artifact_id"),"repository_id":row.get::<String,_>("repository_id"),"path":row.get::<String,_>("path"),"commit":row.get::<String,_>("commit_id"),"blob":row.get::<String,_>("blob_id"),"size_bytes":row.get::<i64,_>("size_bytes").to_string(),"sha256":row.get::<String,_>("content_sha256")})
}
async fn reserve(
    g: &mut UserSessionGuard,
    run: &str,
    f: Provider,
    id: &str,
    req: &str,
    digest: &str,
) -> Result<Option<Value>, UserApiError> {
    if let Some(row)=sqlx::query("SELECT c.request_digest,c.grant_id,a.artifact_id,a.repository_id,a.path,a.commit_id,a.blob_id,a.size_bytes,a.content_sha256 FROM repository_source_calls c LEFT JOIN repository_file_receipts a USING(run_id,provider,request_key,grant_id) WHERE c.run_id=$1 AND c.provider=$2 AND c.request_key=$3")
        .bind(run).bind(f.name()).bind(req).fetch_optional(&mut **g.transaction()).await.map_err(db)?{
        if row.get::<String,_>("request_digest")!=digest||row.get::<String,_>("grant_id")!=id{return Err(UserApiError(StatusCode::CONFLICT,"IDEMPOTENCY_CONFLICT"));}
        if row.get::<Option<String>,_>("artifact_id").is_some(){return Ok(Some(receipt(&row)));}return Err(conflict());
    }
    capacity(g, run).await?;
    sqlx::query("INSERT INTO repository_source_calls(run_id,provider,grant_id,request_key,request_digest,until_at) VALUES($1,$2,$3,$4,$5,clock_timestamp()+interval '35 seconds')").bind(run).bind(f.name()).bind(id).bind(req).bind(digest).execute(&mut **g.transaction()).await.map_err(db)?;
    Ok(None)
}
async fn finalize(
    i: &UserIdentity,
    h: &HeaderMap,
    run: &str,
    f: Provider,
    id: &str,
    req: &str,
    s: &Registered,
) -> Result<UserSessionGuard, UserApiError> {
    if s.reader.authorization_refused() {
        disable_source(i, &s.owner, f.name(), &s.digest).await?;
    }
    let (mut g, _) = guard(i, h, run, true, true).await?;
    live(i, &mut g, run, f, id).await?;
    super::external_grants::complete(&mut g, run, f.name(), id, req).await?;
    Ok(g)
}
pub(super) async fn perform(
    State(i): State<Arc<UserIdentity>>,
    Path((run, name, id)): Path<(String, String, String)>,
    h: HeaderMap,
    body: Result<Json<Action>, JsonRejection>,
) -> Result<Json<Value>, UserApiError> {
    let f = provider(&name)?;
    let Json(a) = body.map_err(|_| AuthError::Invalid)?;
    let req = key(&h)?;
    let digest = orange_protocol::request_digest("repository.read.v1", &(&run, f, &id, &a))
        .map_err(|_| AuthError::Invalid)?;
    let (mut g, _) = guard(&i, &h, &run, true, true).await?;
    let (s, allowed) = live(&i, &mut g, &run, f, &id).await?;
    a.authorize(&s, &allowed)?;
    if let Some(old) = reserve(&mut g, &run, f, &id, req, &digest).await? {
        g.finish().await?;
        return Ok(Json(json!({"kind":"external_untrusted_data","data":old})));
    }
    g.finish().await?;
    let fetched = execute(&s, &allowed, a).await;
    let mut g = finalize(&i, &h, &run, f, &id, req, &s).await?;
    let data = match fetched {
        Err(e) => {
            g.finish().await?;
            return Err(e);
        }
        Ok(Data::Metadata(v)) => v,
        Ok(Data::File(n, b)) => {
            let a = uuid::Uuid::new_v4().to_string();
            let row=sqlx::query("INSERT INTO repository_file_receipts(artifact_id,run_id,provider,grant_id,request_key,repository_id,path,commit_id,blob_id,size_bytes,content_sha256) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *").bind(a).bind(run).bind(name).bind(id).bind(req).bind(n.to_string()).bind(b.path).bind(b.commit).bind(b.oid).bind(b.bytes.len() as i64).bind(b.sha256).fetch_one(&mut **g.transaction()).await.map_err(db)?;
            receipt(&row)
        }
    };
    g.finish().await?;
    Ok(Json(json!({"kind":"external_untrusted_data","data":data})))
}
pub(super) async fn download(
    State(i): State<Arc<UserIdentity>>,
    Path((run, name, id, artifact)): Path<(String, String, String, String)>,
    h: HeaderMap,
) -> Result<Response, UserApiError> {
    let f = provider(&name)?;
    let req = key(&h)?;
    let (mut g, _) = guard(&i, &h, &run, true, true).await?;
    let (s, allowed) = live(&i, &mut g, &run, f, &id).await?;
    let row=sqlx::query("SELECT repository_id,path,commit_id,blob_id,size_bytes,content_sha256 FROM repository_file_receipts WHERE run_id=$1 AND provider=$2 AND grant_id=$3 AND artifact_id=$4").bind(&run).bind(&name).bind(&id).bind(&artifact).fetch_optional(&mut **g.transaction()).await.map_err(db)?.ok_or_else(missing)?;
    let digest =
        orange_protocol::request_digest("repository.download.v1", &(&run, f, &id, &artifact))
            .map_err(|_| AuthError::Invalid)?;
    reserve(&mut g, &run, f, &id, req, &digest).await?;
    g.finish().await?;
    let body = s
        .reader
        .read_file(
            &allowed,
            decimal(row.get("repository_id"))?,
            row.get("commit_id"),
            row.get("path"),
        )
        .await;
    let g = finalize(&i, &h, &run, f, &id, req, &s).await?;
    g.finish().await?;
    let blob = body.map_err(source_error)?;
    if blob.sha256 != row.get::<String, _>("content_sha256")
        || blob.oid != row.get::<String, _>("blob_id")
        || blob.bytes.len() as i64 != row.get::<i64, _>("size_bytes")
    {
        return Err(source_error(Error::Changed));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        blob.bytes,
    )
        .into_response())
}
