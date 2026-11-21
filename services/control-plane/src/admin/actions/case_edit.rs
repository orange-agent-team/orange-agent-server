use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    Assign {
        case_id: String,
        assignee_id: String,
    },
    Note {
        case_id: String,
        note: String,
    },
    RetryMetering {
        case_id: String,
        event_id: String,
    },
    Close {
        case_id: String,
        resolution_ref: String,
    },
}
impl Edit {
    pub(super) fn case_id(&self) -> &str {
        match self {
            Self::Assign { case_id, .. }
            | Self::Note { case_id, .. }
            | Self::RetryMetering { case_id, .. }
            | Self::Close { case_id, .. } => case_id,
        }
    }
    pub(super) fn permission(&self) -> &'static str {
        if matches!(self, Self::RetryMetering { .. }) {
            "metering.retry"
        } else if matches!(self, Self::Close { .. }) {
            "cases.close"
        } else {
            "cases.edit"
        }
    }
    pub(super) fn validate(&self, request: &ActionRequest) -> Result<(), AdminError> {
        let valid = match self {
            Self::Assign { assignee_id, .. } => id(assignee_id),
            Self::Note { note, .. } => {
                reason(note, 1) && !note.chars().any(|c| c.is_control() && c != '\n')
            }
            Self::Close { resolution_ref, .. } => id(resolution_ref),
            Self::RetryMetering { event_id, .. } => {
                request.scope.environment == "dev" && id(event_id)
            }
        };
        if !valid
            || !id(self.case_id())
            || !matches!(request.scope.kind.as_str(), "tenant" | "platform")
        {
            return Err(AdminError::Invalid);
        }
        Ok(())
    }
}
pub(super) async fn target(
    tx: &mut Tx<'_>,
    request: &ActionRequest,
    edit: &Edit,
) -> Result<(i64, String), AdminError> {
    let owner = (request.scope.kind == "tenant").then_some(&request.scope.scope_id);
    let (version,state):(i64,String)=sqlx::query_as("SELECT version,state FROM reconciliation_cases WHERE case_id=$1 AND environment=$2 AND ($3::text IS NULL OR owner_id=$3) FOR UPDATE")
        .bind(edit.case_id()).bind(&request.scope.environment).bind(owner).fetch_optional(&mut **tx).await?.ok_or(AdminError::NotFound)?;
    if request.target_version != version.to_string() {
        return Err(AdminError::Precondition);
    }
    if state == "resolved" {
        return Err(AdminError::Conflict);
    }
    if let Edit::RetryMetering { case_id, event_id } = edit {
        let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM metering_receipts r JOIN reconciliation_cases c USING(case_id,owner_id)
          WHERE r.event_id=$1 AND r.case_id=$2 AND r.state='dead' AND c.business_ref='metering:'||r.event_id AND c.environment='dev')")
          .bind(event_id).bind(case_id).fetch_one(&mut **tx).await?;
        if !allowed {
            return Err(AdminError::Conflict);
        }
    }
    if matches!(edit, Edit::Close { .. }) {
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM metering_receipts WHERE case_id=$1 AND state<>'posted')",
        )
        .bind(edit.case_id())
        .fetch_one(&mut **tx)
        .await?;
        if pending {
            return Err(AdminError::Conflict);
        }
    }
    Ok((version, state))
}
/// Called after locking the case and checking its tenant/environment.
pub(super) async fn assignee_deadline(
    tx: &mut Tx<'_>,
    edit: &Edit,
    scope: &Scope,
) -> Result<i64, AdminError> {
    let Edit::Assign {
        case_id,
        assignee_id,
    } = edit
    else {
        return Ok(i64::MAX);
    };
    let owner = sqlx::query_scalar("SELECT owner_id FROM reconciliation_cases WHERE case_id=$1")
        .bind(case_id)
        .fetch_one(&mut **tx)
        .await?;
    let tenant = Scope {
        environment: scope.environment.clone(),
        kind: "tenant".into(),
        scope_id: owner,
    };
    let platform = Scope {
        environment: scope.environment.clone(),
        kind: "platform".into(),
        scope_id: "platform".into(),
    };
    match actor(tx, assignee_id, &tenant, &["cases.edit"]).await {
        Ok(until) => Ok(until),
        Err(AdminError::Forbidden) => actor(tx, assignee_id, &platform, &["cases.edit"])
            .await
            .map_err(|err| match err {
                AdminError::Forbidden => AdminError::Invalid,
                other => other,
            }),
        Err(error) => Err(error),
    }
}
pub(super) async fn apply(
    tx: &mut Tx<'_>,
    edit: &Edit,
    version: i64,
    state: &str,
    principal: &str,
    action: &str,
    scope: &Scope,
) -> Result<i64, AdminError> {
    if state == "resolved" {
        return Err(AdminError::Conflict);
    }
    let next = version.checked_add(1).ok_or(AdminError::Conflict)?;
    let authority_deadline = assignee_deadline(tx, edit, scope).await?;
    let changed=match edit{
        Edit::Assign{case_id,assignee_id}=>{
            sqlx::query("UPDATE reconciliation_cases SET assigned_to=$1, version=$2 WHERE case_id=$3 AND version=$4 AND assigned_to IS DISTINCT FROM $1")
                .bind(assignee_id).bind(next).bind(case_id).bind(version).execute(&mut **tx).await?
        }
        Edit::Note { case_id,.. } => sqlx::query("UPDATE reconciliation_cases SET version=$1 WHERE case_id=$2 AND version=$3")
            .bind(next).bind(case_id).bind(version).execute(&mut **tx).await?,
        Edit::RetryMetering {case_id,event_id}=>{
            let reopened=sqlx::query("WITH reopened AS (UPDATE metering_receipts r SET state='received',generation=r.generation+1,
                attempts=0,next_attempt=clock_timestamp(),deadline=clock_timestamp()+make_interval(secs=>p.retry_seconds)
                FROM metering_retry_policies p WHERE r.event_id=$1 AND r.case_id=$2 AND r.state='dead' AND r.policy_id=p.policy_id
                RETURNING r.event_id,r.generation)
                INSERT INTO metering_receipt_history(event_id,generation,attempt,stage,reason) SELECT event_id,generation,0,'reopened',$3 FROM reopened")
                .bind(event_id).bind(case_id).bind(action).execute(&mut **tx).await?;
            if reopened.rows_affected()!=1{return Err(AdminError::Conflict);}
            sqlx::query("UPDATE reconciliation_cases SET state='investigating',version=$2 WHERE case_id=$1 AND version=$3")
                .bind(case_id).bind(next).bind(version).execute(&mut **tx).await?
        }
        Edit::Close {case_id,resolution_ref}=>sqlx::query("UPDATE reconciliation_cases SET state='resolved', resolution_ref=$1, version=$2 WHERE case_id=$3 AND version=$4")
            .bind(resolution_ref).bind(next).bind(case_id).bind(version).execute(&mut **tx).await?,
    };
    if changed.rows_affected() != 1 {
        return Err(AdminError::Conflict);
    }
    sqlx::query("INSERT INTO reconciliation_case_edits(action_id,case_id,case_version,actor_id,occurred_at) VALUES($1,$2,$3,$4,$5)").bind(action).bind(edit.case_id()).bind(next).bind(principal).bind(now(tx).await?).execute(&mut **tx).await?;
    Ok(authority_deadline)
}
