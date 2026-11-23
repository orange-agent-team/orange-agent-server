//! Reference relationships, not customer-supplied dispatch authority.
use orange_domain::{Credits, identity::*, next_attempt};
use orange_protocol::marketplace::{ArtifactRef, JobInputRef};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    user: User,
    organization: Option<Organization>,
    job: Job,
    nodes: Vec<Node>,
    artifact: Artifact,
    input: JobInputRef,
    attempts: Vec<Attempt>,
    ledger: Ledger,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    id: String,
    personal_owner: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Organization {
    id: String,
    members: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    id: String,
    owner: String,
    submitter: String,
    model: Model,
    reserved: String,
    duration_limit_ms: String,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Model {
    id: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    id: String,
    owner: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    owner: String,
    reference: ArtifactRef,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    id: String,
    job: String,
    node: String,
    model: Model,
    outcome: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    id: String,
    owner: String,
    job: String,
    winning_attempt: String,
    charged: String,
    entries: Vec<i64>,
}

fn validate(wire: Value) -> Result<(), &'static str> {
    let r: Record = serde_json::from_value(wire).map_err(|_| "wire")?;
    let j = &r.job;
    let owner = OwnerId::try_from(j.owner.as_str()).map_err(|_| "owner")?;
    let user = UserId::try_from(r.user.id.as_str()).map_err(|_| "user")?;
    if j.submitter != user.as_str() {
        return Err("submitter");
    }
    if let Some(org) = r.organization {
        let identity = OrganizationId::try_from(org.id.as_str()).map_err(|_| "organization")?;
        if identity.as_str() != owner.as_str() || !org.members.iter().any(|m| m == user.as_str()) {
            return Err("membership");
        }
    } else if r.user.personal_owner != owner.as_str() {
        return Err("owner");
    }
    if JobId::try_from(j.id.as_str()).is_err() {
        return Err("job");
    }
    if ModelVersionId::try_from(j.model.id.as_str()).is_err() || !valid_digest(&j.model.sha256) {
        return Err("model");
    }
    let reserved = j.reserved.parse::<Credits>().map_err(|_| "credits")?;
    let deadline = j
        .duration_limit_ms
        .parse::<Milliseconds>()
        .map_err(|_| "duration")?;
    if deadline.get() == 0 {
        return Err("duration");
    }
    if r.artifact.owner != j.owner
        || r.artifact.reference.pin().map_err(|_| "artifact")?
            != r.input.pin().map_err(|_| "input")?
    {
        return Err("artifact");
    }
    let mut nodes = BTreeSet::new();
    for node in &r.nodes {
        if NodeId::try_from(node.id.as_str()).is_err()
            || OwnerId::try_from(node.owner.as_str()).is_err()
            || !nodes.insert(&node.id)
        {
            return Err("node");
        }
    }
    let mut seen = BTreeSet::new();
    let mut delivered = None;
    let mut number = 0;
    for a in &r.attempts {
        number = next_attempt(number).map_err(|_| "attempt_limit")?;
        if a.job != j.id || AttemptId::try_from(a.id.as_str()).is_err() || !seen.insert(&a.id) {
            return Err("attempt");
        }
        if !nodes.contains(&a.node) {
            return Err("node");
        }
        if a.model != j.model {
            return Err("model");
        }
        if delivered.is_some() {
            return Err("duplicate_delivery");
        }
        match a.outcome.as_str() {
            "confirmed_prestart_rejection" => {}
            "delivered" => delivered = Some(a.id.as_str()),
            _ => return Err("unconfirmed_attempt"),
        }
    }
    let l = &r.ledger;
    let charged = l.charged.parse::<Credits>().map_err(|_| "credits")?;
    if LedgerTransactionId::try_from(l.id.as_str()).is_err()
        || l.owner != j.owner
        || l.job != j.id
        || delivered != Some(l.winning_attempt.as_str())
        || charged > reserved
    {
        return Err("ledger");
    }
    let debit: i128 = l
        .entries
        .iter()
        .copied()
        .filter(|v| *v < 0)
        .map(i128::from)
        .sum();
    if l.entries.iter().copied().map(i128::from).sum::<i128>() != 0
        || -debit != i128::from(charged.units())
    {
        return Err("postings");
    }
    Ok(())
}

fn examples() -> Vec<Value> {
    serde_json::from_slice(include_bytes!(
        "../../../contracts/fixtures/marketplace-relationships.v1.json"
    ))
    .expect("four fixtures")
}
#[test]
fn four_typed_modalities_and_organized_retry_reconcile() {
    let rows = examples();
    let kinds: BTreeSet<_> = rows
        .iter()
        .map(|r| r["input"]["kind"].as_str().expect("tag"))
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["text", "embedding", "image", "transcription"])
    );
    assert!(
        rows.iter().any(|r| r["organization"].is_object()
            && r["attempts"].as_array().expect("attempts").len() > 1)
    );
    for record in rows {
        assert_eq!(validate(record), Ok(()));
    }
}
#[test]
fn reject_crossed_relationships_and_invalid_amount_or_duration() {
    let rows = examples();
    for (index, pointer, replacement, error) in [
        (0, "/job/owner", json!(""), "owner"),
        (0, "/job/submitter", json!("someone-else"), "submitter"),
        (1, "/organization/members", json!([]), "membership"),
        (0, "/job/id", json!("not/a/job"), "job"),
        (1, "/attempts/1/node", json!("unregistered"), "node"),
        (1, "/attempts/1/id", json!("attempt-2-a"), "attempt"),
        (
            1,
            "/attempts/0/outcome",
            json!("unknown"),
            "unconfirmed_attempt",
        ),
        (
            1,
            "/attempts/0/outcome",
            json!("delivered"),
            "duplicate_delivery",
        ),
        (
            2,
            "/attempts/0/model/sha256",
            json!("a".repeat(64)),
            "model",
        ),
        (
            3,
            "/input/source/version_id",
            json!("other-version"),
            "artifact",
        ),
        (3, "/input/duration_ms", json!("-1"), "input"),
        (0, "/job/duration_limit_ms", json!("1.5"), "duration"),
        (0, "/job/reserved", json!("-7"), "credits"),
        (0, "/ledger/winning_attempt", json!("obsolete"), "ledger"),
        (2, "/ledger/owner", json!("supplier-3"), "ledger"),
        (2, "/ledger/entries", json!([-30, 24, 7]), "postings"),
        (0, "/input/kind", json!("unknown"), "wire"),
    ] {
        let mut row = rows[index].clone();
        *row.pointer_mut(pointer).expect("fixture path") = replacement;
        assert_eq!(validate(row), Err(error), "{pointer}");
    }
    let mut extra = rows[0].clone();
    extra["input"]["source"]["skip_authorization"] = json!(true);
    assert_eq!(validate(extra), Err("wire"));
    let mut oversized = rows[1].clone();
    let mut rejected = oversized["attempts"][0].clone();
    let attempts = oversized["attempts"].as_array_mut().expect("attempt array");
    for id in ["retry-three", "retry-four"] {
        rejected["id"] = json!(id);
        attempts.insert(0, rejected.clone());
    }
    assert_eq!(validate(oversized), Err("attempt_limit"));
    let mut missing = rows[1].clone();
    missing["job"].as_object_mut().expect("job").remove("owner");
    assert_eq!(validate(missing), Err("wire"));
}
