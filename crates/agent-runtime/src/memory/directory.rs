//! Bounded authenticated metadata directory; these DTOs carry no execution grant.
use super::*;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryScopeItem {
    pub scope: MemoryScope,
    pub access: MemoryAccess,
    pub can_administer: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryScopePage {
    pub scopes: Vec<MemoryScopeItem>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryMemberItem {
    pub principal: String,
    pub access: MemoryAccess,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryMemberPage {
    pub scope_id: String,
    pub revision: String,
    pub members: Vec<MemoryMemberItem>,
    pub next_cursor: Option<String>,
}
fn access(raw: &str) -> Result<MemoryAccess, MemoryError> {
    match raw {
        "read" => Ok(MemoryAccess::Read),
        "write" => Ok(MemoryAccess::Write),
        "revoked" => Ok(MemoryAccess::Revoked),
        _ => Err(MemoryError::Invalid),
    }
}
impl Journal {
    /// Actor is supplied by the authenticated Host, never by a model argument.
    /// Disabled scope metadata remains visible to current members so the owner
    /// can manage its state, but authorize_memory still refuses disabled data.
    pub fn list_memory_scopes(
        &self,
        actor: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<MemoryScopePage, MemoryError> {
        if !valid_id(actor) || after.is_some_and(|v| !valid_id(v)) || !(1..=100).contains(&limit) {
            return Err(MemoryError::Invalid);
        }
        let mut q=self.connection.prepare("SELECT s.scope_id,s.owner,s.shared,s.revision,s.enabled,m.access FROM memory_scopes s JOIN memory_members m USING(scope_id) WHERE m.principal=?1 AND m.access IN('read','write') AND (?2 IS NULL OR s.scope_id>?2) ORDER BY s.scope_id LIMIT ?3")?;
        let raw = q
            .query_map(params![actor, after, i64::from(limit) + 1], |r| {
                Ok((
                    MemoryScope {
                        scope_id: r.get(0)?,
                        owner: r.get(1)?,
                        shared: r.get(2)?,
                        revision: r.get::<_, i64>(3)?.to_string(),
                        enabled: r.get(4)?,
                    },
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let more = raw.len() > usize::from(limit);
        let scopes = raw
            .into_iter()
            .take(usize::from(limit))
            .map(|(scope, raw)| {
                Ok(MemoryScopeItem {
                    can_administer: scope.owner == actor,
                    scope,
                    access: access(&raw)?,
                })
            })
            .collect::<Result<Vec<_>, MemoryError>>()?;
        let next_cursor = if more {
            scopes.last().map(|v| v.scope.scope_id.clone())
        } else {
            None
        };
        Ok(MemoryScopePage {
            scopes,
            next_cursor,
        })
    }
    /// The scope owner can inspect the explicit ACL including revoked members.
    /// Ordinary shared readers/writers do not gain a member directory.
    pub fn list_memory_members(
        &self,
        owner: &str,
        scope: &str,
        expected_revision: &str,
        after: Option<&str>,
        limit: u16,
    ) -> Result<MemoryMemberPage, MemoryError> {
        if ![owner, scope].iter().all(|v| valid_id(v))
            || after.is_some_and(|v| !valid_id(v))
            || !(1..=64).contains(&limit)
        {
            return Err(MemoryError::Invalid);
        }
        let expected = version(expected_revision)?;
        let current: Option<i64> = self
            .connection
            .query_row(
                "SELECT revision FROM memory_scopes WHERE scope_id=? AND owner=?",
                params![scope, owner],
                |r| r.get(0),
            )
            .optional()?;
        if current != Some(expected) {
            return Err(MemoryError::Denied);
        }
        let mut q=self.connection.prepare("SELECT principal,access FROM memory_members WHERE scope_id=?1 AND (?2 IS NULL OR principal>?2) ORDER BY principal LIMIT ?3")?;
        let raw = q
            .query_map(params![scope, after, i64::from(limit) + 1], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let more = raw.len() > usize::from(limit);
        let members = raw
            .into_iter()
            .take(usize::from(limit))
            .map(|(principal, raw)| {
                Ok(MemoryMemberItem {
                    principal,
                    access: access(&raw)?,
                })
            })
            .collect::<Result<Vec<_>, MemoryError>>()?;
        let next_cursor = if more {
            members.last().map(|v| v.principal.clone())
        } else {
            None
        };
        Ok(MemoryMemberPage {
            scope_id: scope.into(),
            revision: expected_revision.into(),
            members,
            next_cursor,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_filters_by_current_membership_and_disabled_metadata_never_grants_execution() {
        let root = crate::storage::private_temp();
        let mut j = Journal::open(&root.path().join("journal.sqlite")).expect("open");
        j.create_memory_scope("alice", "a", false).expect("private");
        let shared = j
            .create_memory_scope("alice", "shared", true)
            .expect("shared");
        j.create_memory_scope("bob", "b", false).expect("foreign");
        let shared = j
            .set_memory_member(
                "alice",
                "shared",
                "bob",
                MemoryAccess::Read,
                &shared.revision,
            )
            .expect("share");
        let first = j.list_memory_scopes("alice", None, 1).expect("first");
        assert_eq!(first.scopes[0].scope.scope_id, "a");
        assert_eq!(first.next_cursor.as_deref(), Some("a"));
        assert!(first.scopes[0].can_administer);
        let next = j
            .list_memory_scopes("alice", first.next_cursor.as_deref(), 1)
            .expect("next");
        assert_eq!(next.scopes[0].scope.scope_id, "shared");
        assert!(next.next_cursor.is_none());
        let bob = j.list_memory_scopes("bob", None, 100).expect("bob");
        assert_eq!(
            bob.scopes
                .iter()
                .map(|s| s.scope.scope_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "shared"]
        );
        assert!(!bob.scopes[1].can_administer);
        assert_eq!(bob.scopes[1].access, MemoryAccess::Read);
        let disabled = j
            .set_memory_scope_enabled("alice", "shared", &shared.revision, false)
            .expect("disable");
        assert!(
            !j.list_memory_scopes("alice", None, 100)
                .expect("owner metadata")
                .scopes[1]
                .scope
                .enabled
        );
        assert!(
            j.authorize_memory("alice", "shared", "policy", false)
                .is_err()
        );
        assert!(
            j.authorize_memory("bob", "shared", "policy", false)
                .is_err()
        );
        j.set_memory_member(
            "alice",
            "shared",
            "bob",
            MemoryAccess::Revoked,
            &disabled.revision,
        )
        .expect("remove access");
        assert_eq!(
            j.list_memory_scopes("bob", None, 100)
                .expect("after revoke")
                .scopes
                .len(),
            1
        );
        assert!(
            j.list_memory_scopes("nobody", None, 100)
                .expect("unknown actor")
                .scopes
                .is_empty()
        );
        assert!(j.list_memory_scopes("alice", None, 101).is_err());
    }
    #[test]
    fn member_directory_is_owner_only_bounded_and_revision_checked() {
        let root = crate::storage::private_temp();
        let mut j = Journal::open(&root.path().join("journal.sqlite")).expect("open");
        let scope = j.create_memory_scope("alice", "team", true).expect("scope");
        let scope = j
            .set_memory_member("alice", "team", "bob", MemoryAccess::Write, &scope.revision)
            .expect("member");
        assert!(
            j.list_memory_members("bob", "team", &scope.revision, None, 64)
                .is_err()
        );
        assert!(
            j.list_memory_members("alice", "team", "1", None, 64)
                .is_err()
        );
        assert!(
            j.list_memory_members("alice", "team", &scope.revision, None, 65)
                .is_err()
        );
        let first = j
            .list_memory_members("alice", "team", &scope.revision, None, 1)
            .expect("first");
        assert_eq!(first.members[0].principal, "alice");
        let second = j
            .list_memory_members(
                "alice",
                "team",
                &scope.revision,
                first.next_cursor.as_deref(),
                1,
            )
            .expect("second");
        assert_eq!(second.members[0].principal, "bob");
        assert!(second.next_cursor.is_none());
        let scope = j
            .set_memory_member(
                "alice",
                "team",
                "bob",
                MemoryAccess::Revoked,
                &scope.revision,
            )
            .expect("revoke");
        let members = j
            .list_memory_members("alice", "team", &scope.revision, None, 64)
            .expect("explicit revoked membership");
        assert_eq!(members.members[1].access, MemoryAccess::Revoked);
        assert!(
            j.list_memory_members(
                "alice",
                "team",
                &first.revision,
                first.next_cursor.as_deref(),
                1
            )
            .is_err()
        );
    }
}
