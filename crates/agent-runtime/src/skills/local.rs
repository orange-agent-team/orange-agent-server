use super::*;
pub const LOCAL_SKILL_MAX_BYTES: usize = 16 * 1024;

/// This reads only the manifest. It cannot register tools or execute instructions.
pub fn review_local_skill(bytes: &[u8]) -> Result<SkillTemplate, EngineError> {
    if bytes.len() > LOCAL_SKILL_MAX_BYTES {
        return Err(EngineError::Invalid);
    }
    let template: SkillTemplate =
        serde_json::from_slice(bytes).map_err(|_| EngineError::Invalid)?;
    template.validate()?;
    if !template.id.starts_with("local.") {
        return Err(EngineError::Invalid);
    }
    Ok(template)
}

#[derive(Serialize)]
pub struct LocalSkillReview {
    pub template: SkillTemplate,
    pub pin: SkillPin,
    pub installed: bool,
    pub installation_revision: i64,
    pub activation_revision: i64,
}

#[derive(Default)]
struct Selection {
    digest: String,
    revision: i64,
    enabled: bool,
}

fn selection(db: &Connection, owner: &str, id: &str) -> Result<Selection, EngineError> {
    Ok(db.query_row(
        "SELECT digest,revision,enabled FROM local_skill_selections WHERE owner=? AND package_id=?",
        params![owner,id], |r| Ok(Selection {digest:r.get(0)?,revision:r.get(1)?,enabled:r.get(2)?})
    ).optional()?.unwrap_or_default())
}

fn review(db: &Connection, owner: &str, bytes: &[u8]) -> Result<LocalSkillReview, EngineError> {
    if !valid_id(owner) {
        return Err(EngineError::Invalid);
    }
    let template = review_local_skill(bytes)?;
    let pin = template.pin()?;
    let stored: Option<(String,i64,bool)> = db.query_row(
        "SELECT digest,installation_revision,installed FROM local_skill_packages WHERE owner=? AND package_id=? AND version=?",
        params![owner,pin.id,pin.version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))
    ).optional()?;
    let (installation_revision, installed) = match stored {
        Some((digest, revision, installed)) if digest == pin.digest => (revision, installed),
        Some(_) => return Err(JournalError::Conflict.into()),
        None => (0, false),
    };
    let activation_revision = selection(db, owner, &pin.id)?.revision;
    Ok(LocalSkillReview {
        template,
        pin,
        installed,
        installation_revision,
        activation_revision,
    })
}

fn next_revision(rev: i64) -> Result<i64, EngineError> {
    rev.checked_add(1).ok_or(EngineError::Invalid)
}

fn replay(current: i64, expected: i64) -> bool {
    expected >= 0 && (current == expected || Some(current) == expected.checked_add(1))
}

impl Journal {
    pub fn review_skill_package(
        &self,
        owner: &str,
        bytes: &[u8],
    ) -> Result<LocalSkillReview, EngineError> {
        review(&self.connection, owner, bytes)
    }

    pub fn install_skill_package(
        &mut self,
        owner: &str,
        bytes: &[u8],
        digest: &str,
        expected: i64,
    ) -> Result<i64, EngineError> {
        let db = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let review = review(&db, owner, bytes)?;
        let pin = review.pin;
        if pin.digest != digest || expected < 0 {
            return Err(EngineError::Invalid);
        }
        let current = review.installation_revision;
        if review.installed && replay(current, expected) {
            return Ok(current);
        }
        if current != expected {
            return Err(JournalError::Conflict.into());
        }
        let next = next_revision(current)?;
        let document = serde_json::to_string(&review.template).map_err(|_| EngineError::Invalid)?;
        db.execute("INSERT INTO local_skill_packages(owner,package_id,version,digest,installation_revision,document,installed)
          VALUES(?,?,?,?,?,?,1) ON CONFLICT(owner,package_id,version) DO UPDATE SET document=excluded.document,
          installed=1,installation_revision=excluded.installation_revision",params![owner,pin.id,pin.version,pin.digest,next,document])?;
        db.commit()?;
        Ok(next)
    }

    pub fn uninstall_skill_package(
        &mut self,
        owner: &str,
        pin: &SkillPin,
        expected: i64,
        expected_activation: i64,
    ) -> Result<i64, EngineError> {
        if !valid_id(owner)
            || !pin.id.starts_with("local.")
            || expected < 0
            || expected_activation < 0
        {
            return Err(EngineError::Invalid);
        }
        let db = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, installed): (i64,bool)=db.query_row(
           "SELECT installation_revision,installed FROM local_skill_packages WHERE owner=? AND package_id=? AND version=? AND digest=?",
           params![owner,pin.id,pin.version,pin.digest],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(EngineError::Forbidden)?;
        if !installed && replay(current, expected) {
            return Ok(current);
        }
        let active = selection(&db, owner, &pin.id)?;
        if current != expected || active.revision != expected_activation {
            return Err(JournalError::Conflict.into());
        }
        if active.enabled && active.digest == pin.digest {
            db.execute("UPDATE local_skill_selections SET enabled=0,revision=? WHERE owner=? AND package_id=?",
             params![next_revision(active.revision)?,owner,pin.id])?;
        }
        let next = next_revision(current)?;
        db.execute("UPDATE local_skill_packages SET installed=0,installation_revision=?1,document=CASE WHEN EXISTS(
            SELECT 1 FROM local_run_skills WHERE owner=?2 AND package_id=?3 AND digest=?4) THEN document ELSE NULL END
            WHERE owner=?2 AND package_id=?3 AND digest=?4",params![next,owner,pin.id,pin.digest])?;
        db.commit()?;
        Ok(next)
    }

    pub(super) fn local_skills(&self, owner: &str) -> Result<Vec<SkillStatus>, EngineError> {
        let mut stmt=self.connection.prepare("SELECT p.document,p.digest,coalesce(s.enabled AND s.digest=p.digest,0),coalesce(s.revision,0)
          FROM local_skill_packages p LEFT JOIN local_skill_selections s USING(owner,package_id)
          WHERE p.owner=? AND p.installed ORDER BY p.package_id,p.version")?;
        let rows = stmt.query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (doc, digest, enabled, revision) = row?;
            let template = pinned(&doc, &digest)?;
            result.push(SkillStatus {
                required_model_profile: template.required_model_profile(),
                pin: template.pin()?,
                name: template.name,
                scenario: template.scenario,
                tools: template.tools,
                instruction: template.instruction,
                enabled,
                revision,
            });
        }
        Ok(result)
    }

    pub(super) fn set_local_skill_enabled(
        &mut self,
        owner: &str,
        pin: &SkillPin,
        expected: i64,
        enable: bool,
    ) -> Result<i64, EngineError> {
        let db = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let doc:String=db.query_row("SELECT document FROM local_skill_packages WHERE owner=? AND package_id=? AND version=? AND digest=? AND installed",
          params![owner,pin.id,pin.version,pin.digest],|r|r.get(0)).optional()?.ok_or(EngineError::Forbidden)?;
        if pinned(&doc, &pin.digest)?.pin()? != *pin {
            return Err(EngineError::RecoveryRequired);
        }
        let active = selection(&db, owner, &pin.id)?;
        if active.digest == pin.digest
            && active.enabled == enable
            && replay(active.revision, expected)
        {
            return Ok(active.revision);
        }
        if active.revision != expected {
            return Err(JournalError::Conflict.into());
        }
        if !enable && (!active.enabled || active.digest != pin.digest) {
            return Ok(active.revision);
        }
        let next = next_revision(active.revision)?;
        db.execute("INSERT INTO local_skill_selections(owner,package_id,digest,revision,enabled) VALUES(?,?,?,?,?)
          ON CONFLICT(owner,package_id) DO UPDATE SET digest=excluded.digest,revision=excluded.revision,enabled=excluded.enabled",
          params![owner,pin.id,pin.digest,next,enable])?;
        db.commit()?;
        Ok(next)
    }
}
