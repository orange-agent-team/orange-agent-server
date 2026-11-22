//! Digest-pinned declarative templates. Enabling a template never grants access to a workspace.
use crate::{Journal, JournalError, catalog::ToolCatalog, engine::EngineError};
use orange_protocol::{
    model::{Message, Role},
    request_digest, valid_id,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPin {
    pub id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillTemplate {
    pub id: String,
    pub version: String,
    pub scenario: String,
    pub name: String,
    pub instruction: String,
    pub tools: Vec<String>,
}

impl SkillTemplate {
    pub fn pin(&self) -> Result<SkillPin, EngineError> {
        self.validate()?;
        let digest =
            request_digest("orange.skill.template.v1", self).map_err(|_| EngineError::Invalid)?;
        Ok(SkillPin {
            id: self.id.clone(),
            version: self.version.clone(),
            digest,
        })
    }
    fn validate(&self) -> Result<(), EngineError> {
        let version = self.version.split('.').collect::<Vec<_>>();
        let unique = self.tools.iter().collect::<BTreeSet<_>>();
        let valid_namespace = self.id.strip_prefix("orange.").is_some_and(|name| {
            name.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && name.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')
                })
        });
        if !valid_namespace
            || !valid_id(&self.id)
            || self.scenario.len() != 5
            || !self.scenario.starts_with("AU-")
            || !self.scenario.as_bytes()[3..].iter().all(u8::is_ascii_digit)
            || version.len() != 3
            || !version
                .iter()
                .all(|v| v.parse::<u32>().is_ok_and(|n| n.to_string() == **v))
            || self.instruction.trim().is_empty()
            || self.instruction.len() > 8192
            || self.name.trim().is_empty()
            || self.name.len() > 160
            || self.tools.len() > 16
            || unique.len() != self.tools.len()
            || !self.tools.iter().all(|t| {
                matches!(
                    t.as_str(),
                    "read_file" | "list_directory" | "search_files" | "search_text"
                )
            })
        {
            return Err(EngineError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn messages(&self, input: String) -> Result<Vec<Message>, EngineError> {
        if input.trim().is_empty() || input.len() > 65536 {
            return Err(EngineError::Invalid);
        }
        Ok(vec![
            Message::plain(Role::System, self.instruction.clone()),
            Message::plain(Role::User, input),
        ])
    }
    pub(crate) fn limit_catalog(&self, catalog: ToolCatalog) -> Result<ToolCatalog, EngineError> {
        let specs = self
            .tools
            .iter()
            .map(|name| {
                catalog
                    .describe(name)
                    .cloned()
                    .ok_or(EngineError::RecoveryRequired)
            })
            .collect::<Result<Vec<_>, _>>()?;
        ToolCatalog::new(specs).map_err(|_| EngineError::RecoveryRequired)
    }
}

pub fn bundled() -> Result<Vec<SkillTemplate>, EngineError> {
    let templates: Vec<SkillTemplate> =
        serde_json::from_str(include_str!("skills/bundled.v1.json"))
            .map_err(|_| EngineError::Invalid)?;
    let mut ids = BTreeSet::new();
    for entry in &templates {
        entry.validate()?;
        if !ids.insert(&entry.id) {
            return Err(EngineError::Invalid);
        }
    }
    let released: Vec<SkillPin> = serde_json::from_str(include_str!("skills/release.v1.json"))
        .map_err(|_| EngineError::Invalid)?;
    if templates
        .iter()
        .map(SkillTemplate::pin)
        .collect::<Result<Vec<_>, _>>()?
        != released
    {
        return Err(EngineError::RecoveryRequired);
    }
    Ok(templates)
}

#[derive(Debug, Serialize)]
pub struct SkillStatus {
    pub pin: SkillPin,
    pub scenario: String,
    pub name: String,
    pub tools: Vec<String>,
    pub instruction: String,
    pub enabled: bool,
    pub revision: i64,
}

pub(crate) struct BoundSkill {
    pub template: SkillTemplate,
    pub current: bool,
}

fn pinned(doc: &str, digest: &str) -> Result<SkillTemplate, EngineError> {
    let template: SkillTemplate =
        serde_json::from_str(doc).map_err(|_| EngineError::RecoveryRequired)?;
    if template.pin()?.digest != digest {
        return Err(EngineError::RecoveryRequired);
    }
    Ok(template)
}

pub(crate) fn for_run(db: &Connection, run: &str) -> Result<Option<BoundSkill>, EngineError> {
    let row:Option<(String,String,bool)>=db.query_row("SELECT v.document,b.digest,coalesce(s.enabled AND s.revision=b.activation_revision AND s.digest=b.digest,0)
      FROM runtime_skill_bindings b JOIN skill_versions v USING(digest) JOIN runs r USING(run_id)
      LEFT JOIN owner_skills s ON s.owner=r.owner AND s.package_id=v.package_id WHERE b.run_id=?", [run],
      |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    row.map(|(doc, digest, current)| {
        Ok(BoundSkill {
            template: pinned(&doc, &digest)?,
            current,
        })
    })
    .transpose()
}

impl Journal {
    pub fn skills(&self, owner: &str) -> Result<Vec<SkillStatus>, EngineError> {
        if !valid_id(owner) {
            return Err(EngineError::Invalid);
        }
        let mut result = Vec::new();
        for template in bundled()? {
            let pin = template.pin()?;
            let status:Option<(String,i64,bool)>=self.connection.query_row(
                "SELECT digest,revision,enabled FROM owner_skills WHERE owner=? AND package_id=?",
                params![owner,template.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let enabled = status
                .as_ref()
                .is_some_and(|(digest, _, active)| *active && digest == &pin.digest);
            let revision = status.map_or(0, |(_, revision, _)| revision);
            result.push(SkillStatus {
                pin,
                scenario: template.scenario,
                name: template.name,
                tools: template.tools,
                instruction: template.instruction,
                enabled,
                revision,
            });
        }
        Ok(result)
    }
    pub fn set_skill_enabled(
        &mut self,
        owner: &str,
        pin: &SkillPin,
        expected: i64,
        enable: bool,
    ) -> Result<i64, EngineError> {
        if !valid_id(owner) || expected < 0 {
            return Err(EngineError::Invalid);
        }
        let template = bundled()?
            .into_iter()
            .find(|t| t.id == pin.id && t.version == pin.version)
            .ok_or(EngineError::Invalid)?;
        if template.pin()? != *pin {
            return Err(EngineError::Invalid);
        }
        let db = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<(String, i64, bool)> = db
            .query_row(
                "SELECT digest,revision,enabled FROM owner_skills WHERE owner=? AND package_id=?",
                params![owner, pin.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let revision = old.as_ref().map_or(0, |(_, r, _)| *r);
        if old
            .as_ref()
            .is_some_and(|(d, _, active)| d == &pin.digest && *active == enable)
            && (expected == revision || expected.checked_add(1) == Some(revision))
        {
            db.commit()?;
            return Ok(revision);
        }
        if expected != revision {
            return Err(JournalError::Conflict.into());
        }
        if old.is_none() && !enable {
            db.commit()?;
            return Ok(0);
        }
        let next = revision.checked_add(1).ok_or(EngineError::Invalid)?;
        let json = serde_json::to_string(&template).map_err(|_| EngineError::Invalid)?;
        db.execute(
            "INSERT INTO skill_versions VALUES(?,?,?,?) ON CONFLICT(digest) DO NOTHING",
            params![pin.digest, pin.id, pin.version, json],
        )?;
        db.execute("INSERT INTO owner_skills VALUES(?,?,?,?,?) ON CONFLICT(owner,package_id) DO UPDATE SET digest=excluded.digest,revision=excluded.revision,enabled=excluded.enabled",
             params![owner,pin.id,pin.digest,next,enable])?;
        db.commit()?;
        Ok(next)
    }
    pub fn run_skill(&self, owner: &str, run: &str) -> Result<Option<SkillPin>, EngineError> {
        let owns: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE owner=? AND run_id=?)",
            params![owner, run],
            |r| r.get(0),
        )?;
        if !owns {
            return Err(EngineError::Forbidden);
        }
        for_run(&self.connection, run)?
            .map(|b| b.template.pin())
            .transpose()
    }
    pub(crate) fn resolve_skill(
        &self,
        owner: &str,
        pin: &SkillPin,
    ) -> Result<SkillTemplate, EngineError> {
        let doc:Option<String>=self.connection.query_row("SELECT v.document FROM owner_skills s JOIN skill_versions v USING(digest) WHERE s.owner=? AND s.package_id=? AND v.version=? AND s.digest=? AND s.enabled",
            params![owner,pin.id,pin.version,pin.digest],|r|r.get(0)).optional()?;
        pinned(&doc.ok_or(EngineError::Forbidden)?, &pin.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceReference {
    pub path: String,
    pub sha256: String,
    pub start_byte: String,
    pub end_byte: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

pub(crate) fn source_references(
    db: &Connection,
    run: &str,
) -> Result<Vec<SourceReference>, EngineError> {
    use crate::workspace::WorkspaceResult;
    let mut stmt=db.prepare("SELECT result_json FROM operations WHERE run_id=? AND state='succeeded' AND context_applied=1
       AND json_extract(request_json,'$.tool') IN ('read_file','search_text') ORDER BY rowid")?;
    let mut refs = BTreeSet::new();
    for row in stmt.query_map([run], |r| r.get::<_, String>(0))? {
        match serde_json::from_str::<WorkspaceResult>(&row?)
            .map_err(|_| EngineError::RecoveryRequired)?
        {
            WorkspaceResult::ReadFile(r) => {
                refs.insert(SourceReference {
                    path: r.path,
                    sha256: r.sha256,
                    start_byte: r.start_byte,
                    end_byte: r.end_byte,
                    line: None,
                });
            }
            WorkspaceResult::SearchText(r) => {
                for m in r.matches {
                    refs.insert(SourceReference {
                        path: m.path,
                        sha256: m.sha256,
                        start_byte: m.start_byte,
                        end_byte: m.end_byte,
                        line: Some(m.line),
                    });
                }
            }
            _ => return Err(EngineError::RecoveryRequired),
        }
    }
    Ok(refs.into_iter().collect())
}
